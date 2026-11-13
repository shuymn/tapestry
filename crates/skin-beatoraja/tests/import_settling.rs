//! Critical untimed tracks that settle (D3): each element kind that
//! reaches the rule plays its authored keys on `screen` and holds the
//! last, against the same destination authored as one key at each pose
//! it reaches, with one D3 note; and what still refuses, each with its
//! own note.

mod common;

use common::lanes::{NOTES, body, hidden, hidden_object, lift_object, note, with};
use common::play::{Drawn, judged, tally};
use common::{Loaded, d3_notes, import_body, import_config};
use rules::{GaugeValue, Judgment};
use skin_beatoraja::{
    Config, CriticalPart, DeviationId, ImportError, NoteKind, OffsetValue, Severity, TimerWhy,
    TrackWhy,
};

/// The first key.
const A: &str = r#""x": 20, "y": 800, "w": 430, "h": 600"#;
/// Halfway from `A` to `B`.
const MID: &str = r#""x": 20, "y": 750, "w": 430, "h": 550"#;
/// The last key: moved and resized.
const B: &str = r#""x": 20, "y": 700, "w": 430, "h": 500"#;

/// `A` at 0 and `B` at 500 under `loop`.
fn moving(looped: i32) -> String {
    format!(r#""loop": {looped}, "dst": [{{"time": 0, {A}}}, {{"time": 500, {B}}}]"#)
}

/// `pose` alone: what a settling track must show where it reaches it.
fn still(pose: &str) -> String {
    format!(r#""dst": [{{{pose}}}]"#)
}

/// The rects shown at `ms` + 1, just after a judgment at `ms`, with
/// SUDDEN+ at 500 ‰.
fn rects(l: &Loaded, ms: i64) -> Vec<[i32; 4]> {
    let mut d = Drawn::new(l.ok().skin.clone(), 100);
    d.view.cover = 500;
    d.tally = tally(0, 12);
    d.frame(ms, &[judged(ms, Judgment::Perfect, 12)]);
    d.shown(ms + 1)
}

#[track_caller]
fn refusal(l: Loaded) -> (CriticalPart, NoteKind) {
    match l.err() {
        ImportError::Critical { part, why, .. } => (part, why),
        e => panic!("{e:?}"),
    }
}

/// The D3 notes of `l` at their destinations.
fn d3_at(l: &Loaded) -> Vec<Option<skin_beatoraja::NoteLoc>> {
    let d3 = NoteKind::Deviation(DeviationId::D3);
    l.report()
        .notes
        .iter()
        .filter(|n| n.kind == d3)
        .map(|n| n.at)
        .collect()
}

/// Imports with one located D3 note, and shows at each `(ms, pose)` what
/// `pose`, authored alone, shows there: its entrance played, then held.
#[track_caller]
fn assert_plays(kind: fn(&str) -> Loaded, track: &str, poses: &[(i64, &str)], case: &str) {
    let l = kind(track);
    let note = l
        .report()
        .notes
        .iter()
        .find(|n| n.kind == NoteKind::Deviation(DeviationId::D3));
    assert_eq!(d3_notes(&l), 1, "{case}");
    assert_eq!(
        note.map(|n| n.severity),
        Some(Severity::Deviation),
        "{case}"
    );
    assert!(note.is_some_and(|n| n.at.is_some()), "{case}");
    for &(ms, pose) in poses {
        assert_eq!(rects(&l, ms), rects(&kind(pose), ms), "{case} at {ms}");
    }
}

/// A track settles at `loop` on its last key, and past it where its first
/// key shows the last's values; it plays from its first key, a first key
/// after 0 included, and holds the last.
fn settling(kind: fn(&str) -> Loaded) {
    let (a, mid, b) = (still(A), still(MID), still(B));
    // The poses differ, so a frozen track cannot pass.
    assert_ne!(rects(&kind(&a), 0), rects(&kind(&b), 0));
    assert_ne!(rects(&kind(&mid), 0), rects(&kind(&b), 0));
    let at_last = [(0, a.as_str()), (249, &mid), (500, &b), (5_000, &b)];
    assert_plays(kind, &moving(500), &at_last, "loop at the last key");
    let back = format!(
        r#""loop": 900, "dst": [{{"time": 0, {B}}}, {{"time": 250, {A}}}, {{"time": 500, {B}}}]"#
    );
    let past = [(124, mid.as_str()), (249, &a), (500, &b), (5_000, &b)];
    assert_plays(kind, &back, &past, "loop past the last key");
    let late = format!(r#""loop": 500, "dst": [{{"time": 100, {A}}}, {{"time": 500, {B}}}]"#);
    let unseen = format!(r#""dst": [{{"time": 100, {B}}}]"#);
    let from_100 = [(49, unseen.as_str()), (299, &mid), (500, &b), (5_000, &b)];
    assert_plays(kind, &late, &from_100, "a first key after 0");
    // Two keys at the last time: the later one is the last key.
    let pair = format!(
        r#""loop": 500, "dst": [{{"time": 0, {A}}}, {{"time": 500, {A}}}, {{"time": 500, {B}}}]"#
    );
    let jump = [(249, a.as_str()), (500, &b), (5_000, &b)];
    assert_plays(kind, &pair, &jump, "two keys at the last time");
}

/// A `loop` before the last key repeats and one of -1 hides, each still
/// refused as untimed; a `loop` past the last key shows the first key.
fn not_settling(kind: fn(&str) -> Loaded, part: CriticalPart) {
    let untimed = (part, NoteKind::Timer(TimerWhy::Untimed));
    assert_eq!(refusal(kind(&moving(200))), untimed, "before the last key");
    assert_eq!(refusal(kind(&moving(0))), untimed, "at the first key");
    assert_eq!(refusal(kind(&moving(-1))), untimed, "hidden after its last");
    assert_eq!(
        refusal(kind(&moving(900))),
        (part, NoteKind::Track(TrackWhy::FallbackToFirst)),
        "past the last key"
    );
}

/// On a timer of its own a track plays as authored, with no note.
fn timed(kind: fn(&str) -> Loaded) {
    let l = kind(&format!(r#""timer": 41, {}"#, moving(500)));
    assert_eq!(d3_notes(&l), 0);
    let (play_start, moved) = (rects(&l, 100), rects(&l, 5_000));
    assert!(!moved.is_empty());
    assert_ne!(play_start, moved);
}

/// A gauge's images and record.
const GAUGE: &str = r#""image": [{"id": "n0", "src": "a", "w": 8, "h": 8},
                                 {"id": "n1", "src": "a", "w": 8, "h": 8},
                                 {"id": "n2", "src": "a", "w": 8, "h": 8},
                                 {"id": "n3", "src": "a", "w": 8, "h": 8}],
                       "gauge": {"id": "g", "nodes": ["n0", "n1", "n2", "n3"], "parts": 10}"#;

fn gauge(track: &str) -> Loaded {
    import_body(&format!(
        r#"{GAUGE}, "destination": [{{"id": "g", {track}}}]"#
    ))
}

fn number(track: &str) -> Loaded {
    import_body(&format!(
        r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4,
                       "ref": 71}}],
           "destination": [{{"id": "v", {track}}}]"#
    ))
}

fn cover_image(track: &str) -> Loaded {
    with(
        "",
        &[&format!(r#"{{"id": "cover", "offset": 4, {track}}}"#)],
    )
}

fn cover_system(track: &str) -> Loaded {
    with("", &[&format!(r#"{{"id": "-110", "offset": 4, {track}}}"#)])
}

fn cover_image_set(track: &str) -> Loaded {
    with(
        r#""imageset": [{"id": "s", "images": ["cover"], "value": 110}]"#,
        &[&format!(r#"{{"id": "s", "offset": 4, {track}}}"#)],
    )
}

fn cover_image_group(track: &str) -> Loaded {
    let group = r#"{"id": "s", "src": "a", "w": 16, "h": 16, "divx": 4, "len": 2}"#;
    let d = format!(r#"{{"id": "s", "offset": 4, {track}}}"#);
    import_body(&body(group, &note(""), &[NOTES, &d]))
}

fn cover_slider(track: &str) -> Loaded {
    with(
        r#""slider": [{"id": "sl", "src": "a", "x": 0, "y": 24, "w": 16, "h": 16,
                       "type": 4, "angle": 2, "range": 600}]"#,
        &[&format!(r#"{{"id": "sl", {track}}}"#)],
    )
}

fn lift(track: &str) -> Loaded {
    with(
        &lift_object(""),
        &[&format!(r#"{{"id": "liftc", {track}}}"#)],
    )
}

/// A judge of `grade` and `combo`, its outer destination unmoving.
fn judge(grade: &str, combo: &str) -> Loaded {
    import_body(&format!(
        r#""image": [{{"id": "g0", "src": "a", "w": 8, "h": 8}}],
           "value": [{{"id": "ten", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 3}}],
           "judge": [{{"id": "j", "index": 0, "images": [{grade}], "numbers": [{combo}]}}],
           "destination": [{{"id": "j", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}]"#
    ))
}

fn judge_grade(track: &str) -> Loaded {
    judge(&format!(r#"{{"id": "g0", {track}}}"#), "")
}

fn judge_combo(track: &str) -> Loaded {
    let grade = r#"{"id": "g0", "dst": [{"x": 100, "y": 500, "w": 80, "h": 20}]}"#;
    judge(grade, &format!(r#"{{"id": "ten", {track}}}"#))
}

/// The three behaviours for each element kind that reaches the rule, each
/// its own test so that a change shows every kind it breaks.
macro_rules! each_kind {
    ($($kind:ident: $part:expr;)*) => {
        $(mod $kind {
            use super::*;

            #[test]
            fn settling_plays_its_entrance() {
                settling(super::$kind);
            }

            #[test]
            fn not_settling_refuses() {
                not_settling(super::$kind, $part);
            }

            #[test]
            fn timed_plays_as_authored() {
                timed(super::$kind);
            }
        })*
    };
}

each_kind! {
    gauge: CriticalPart::Gauge;
    number: CriticalPart::ExScore;
    cover_image: CriticalPart::Cover;
    cover_system: CriticalPart::Cover;
    cover_image_set: CriticalPart::Cover;
    cover_image_group: CriticalPart::Cover;
    cover_slider: CriticalPart::Cover;
    lift: CriticalPart::Lift;
    judge_grade: CriticalPart::Judge;
    judge_combo: CriticalPart::Judge;
}

/// The red of each quad at `ms`.
fn reds(l: Loaded, ms: i64) -> Vec<f32> {
    l.quads(ms).iter().map(|q| q.color[0]).collect()
}

#[test]
fn a_colour_channel_settles_as_a_move_does() {
    let tinted =
        format!(r#""loop": 100, "dst": [{{"time": 0, {B}}}, {{"time": 100, {B}, "r": 0}}]"#);
    assert_eq!(d3_notes(&number(&tinted)), 1);
    // 255 at 0, half of it at 50, 0 from 100 on.
    assert_eq!(reds(number(&tinted), 0), [1.0]);
    assert_eq!(reds(number(&tinted), 50), [127.5 / 255.0]);
    assert_eq!(reds(number(&tinted), 101), [0.0]);
    assert_eq!(reds(number(&tinted), 5_000), [0.0]);
    let looping = tinted.replace(r#""loop": 100"#, r#""loop": 50"#);
    assert_eq!(
        refusal(number(&looping)),
        (CriticalPart::ExScore, NoteKind::Timer(TimerWhy::Untimed))
    );
}

/// The alpha of each quad at `ms`.
fn alphas(l: Loaded, ms: i64) -> Vec<f32> {
    l.quads(ms).iter().map(|q| q.color[3]).collect()
}

/// An alpha in a moving track runs with its pose; a track changing only
/// its alpha keeps its fade on `screen` (D3) as before.
#[test]
fn alpha_runs_with_the_pose_and_a_fade_alone_stays_d3() {
    let track = format!(
        r#""loop": 500, "dst": [{{"time": 0, {A}, "a": 0}}, {{"time": 500, {B}, "a": 102}}]"#
    );
    assert_eq!(d3_notes(&gauge(&track)), 1);
    assert_eq!(alphas(gauge(&track), 250), [0.2; 10]);
    assert_eq!(alphas(gauge(&track), 501), [0.4; 10]);

    let fade = format!(
        r#""loop": 500, "dst": [{{"time": 0, {B}, "a": 0}}, {{"time": 500, {B}, "a": 255}}]"#
    );
    assert_eq!(d3_notes(&gauge(&fade)), 1);
    assert_eq!(alphas(gauge(&fade), 250), [0.5; 10]);
}

/// A change only on a key between two at one time, which never draws,
/// still counts as a change (`SkinObject.java:198-216`): a pose there
/// makes the track settle and an alpha there keeps its fade, each noted
/// once, though neither ever shows.
#[test]
fn a_change_on_a_key_that_never_draws_still_counts() {
    let plain = format!(r#""loop": 500, "dst": [{{"time": 0, {B}}}, {{"time": 500, {B}}}]"#);
    let posed = format!(
        r#""loop": 500, "dst": [{{"time": 0, {B}}}, {{"time": 500, {B}}}, {{"time": 500, {A}}}, {{"time": 500, {B}}}]"#
    );
    let faded = format!(
        r#""loop": 500, "dst": [{{"time": 0, {B}}}, {{"time": 500, {B}}}, {{"time": 500, {B}, "a": 100}}, {{"time": 500, {B}}}]"#
    );
    assert_eq!(d3_notes(&gauge(&plain)), 0);
    for track in [&posed, &faded] {
        let l = gauge(track);
        assert_eq!(d3_notes(&l), 1, "{track}");
        for ms in [0, 250, 500, 5_000] {
            assert_eq!(rects(&l, ms), rects(&gauge(&plain), ms), "{track} at {ms}");
        }
    }
}

/// A gauge in `track` under offset 41, set to `v`.
fn offset_gauge(track: &str, v: OffsetValue) -> Loaded {
    import_config(
        [1_920, 1_080],
        &format!(
            r#""offset": [{{"name": "b", "id": 41, "x": true, "a": true}}], {GAUGE},
               "destination": [{{"id": "g", "offset": 41, {track}}}]"#
        ),
        &[],
        |c: &mut Config| {
            c.offsets.insert(41, v);
        },
    )
}

/// The offset `x` 7, alpha −102.
const SHIFTED: OffsetValue = OffsetValue {
    x: 7,
    y: 0,
    w: 0,
    h: 0,
    r: 0,
    a: -102,
};

/// With a fixed colour an offset applies at every key, the source's
/// colour then never interpolating (`SkinObject.java:480-489`).
#[test]
fn an_offset_applies_to_every_authored_key() {
    let l = offset_gauge(&moving(500), SHIFTED);
    assert_eq!(d3_notes(&l), 1);
    for (ms, pose) in [(0, A), (249, MID), (500, B), (5_000, B)] {
        let shifted = rects(&offset_gauge(&still(pose), SHIFTED), ms);
        assert_eq!(rects(&l, ms), shifted, "{ms}");
        let plain = rects(&offset_gauge(&still(pose), OffsetValue::default()), ms);
        assert_ne!(rects(&l, ms), plain, "{ms}");
    }
    for ms in [0, 250, 5_000] {
        assert_eq!(alphas(offset_gauge(&moving(500), SHIFTED), ms), [0.6; 10]);
    }
}

/// A move that also fades needs its alpha offset skipped while the
/// colour interpolates and applied at its keys (`SkinObject.java:490-519`),
/// which the model cannot do: it refuses with the offset's note, settling
/// or looping.
#[test]
fn a_fading_move_with_an_alpha_offset_refuses() {
    let fading = format!(
        r#""loop": 500, "dst": [{{"time": 0, {A}, "a": 0}}, {{"time": 500, {B}, "a": 255}}]"#
    );
    let offset_alpha = (CriticalPart::Gauge, NoteKind::OffsetAlpha);
    assert_eq!(refusal(offset_gauge(&fading, SHIFTED)), offset_alpha);
    let looping = fading.replace(r#""loop": 500"#, r#""loop": 200"#);
    assert_eq!(refusal(offset_gauge(&looping, SHIFTED)), offset_alpha);
    // Without the offset it plays.
    assert_eq!(d3_notes(&offset_gauge(&fading, OffsetValue::default())), 1);
}

/// A gauge's lit segments follow the live value through its entrance.
#[test]
fn a_settling_gauge_shows_the_live_value() {
    let skin = |track: &str| {
        import_body(&format!(
            r#""image": [{{"id": "n0", "src": "a", "x": 0, "w": 8, "h": 8}},
                         {{"id": "n1", "src": "a", "x": 8, "w": 8, "h": 8}},
                         {{"id": "n2", "src": "a", "x": 16, "w": 8, "h": 8}},
                         {{"id": "n3", "src": "a", "x": 24, "w": 8, "h": 8}}],
               "gauge": {{"id": "g", "nodes": ["n0", "n1", "n2", "n3"], "parts": 10}},
               "destination": [{{"id": "g", {track}}}]"#
        ))
    };
    let settling = skin(&moving(500));
    assert_eq!(d3_notes(&settling), 1);
    let at = |l: &Loaded, ms: i64, percent: i64| {
        let mut d = Drawn::new(l.ok().skin.clone(), 100);
        d.gauges[0].value = GaugeValue(percent * GaugeValue::PERCENT);
        d.frame(ms, &[])
    };
    for (ms, pose) in [(250, MID), (1_000, B)] {
        assert_ne!(at(&settling, ms, 20), at(&settling, ms, 80));
        for percent in [0, 20, 80, 100] {
            let posed = at(&skin(&still(pose)), ms, percent);
            assert_eq!(at(&settling, ms, percent), posed, "{ms} {percent}");
        }
    }
}

/// A last key of alpha 0 or of no size shows nothing once the entrance
/// ends, as at the source's rest; the entrance itself shows.
#[test]
fn a_final_pose_nothing_shows_ends_invisible() {
    let hidden = |q: &skin_model::Quad| q.color[3] == 0.0 || q.rect[2] <= 0.0 || q.rect[3] <= 0.0;
    for (name, last) in [
        ("alpha", r#""x": 20, "y": 700, "w": 430, "h": 500, "a": 0"#),
        ("height", r#""x": 20, "y": 700, "w": 430, "h": 0"#),
        ("width", r#""x": 20, "y": 700, "w": 0, "h": 500"#),
    ] {
        let track = format!(r#""loop": 500, "dst": [{{"time": 0, {A}}}, {{"time": 500, {last}}}]"#);
        let l = gauge(&track);
        assert_eq!(d3_notes(&l), 1, "{name}");
        let at = |ms| Drawn::new(l.ok().skin.clone(), 100).frame(ms, &[]);
        let entering = at(250);
        assert!(!entering.iter().any(hidden), "{name}: {entering:?}");
        for ms in [500, 5_000] {
            let seen = at(ms);
            assert!(seen.iter().all(hidden), "{name} at {ms}: {seen:?}");
        }
    }
}

const WIDE: &str = r#""x": 20, "y": 800, "w": 600, "h": 300"#;
const WIDER: &str = r#""x": 20, "y": 800, "w": 700, "h": 300"#;
const WIDE_MID: &str = r#""x": 20, "y": 800, "w": 650, "h": 300"#;
const TALL: &str = r#""x": 20, "y": 800, "w": 300, "h": 600"#;

/// A stretched cover from `from` at 0 to `to` at 500, settling.
fn stretched(mode: i32, from: &str, to: &str) -> Loaded {
    cover_image(&format!(
        r#""stretch": {mode}, "loop": 500, "dst": [{{"time": 0, {from}}}, {{"time": 500, {to}}}]"#
    ))
}

/// Stretch 1 picks its branch per frame: keys taking different ones
/// refuse with the stretch's note.
#[test]
fn a_stretch_crossing_a_branch_refuses() {
    assert_eq!(
        refusal(stretched(1, WIDE, TALL)),
        (CriticalPart::Cover, NoteKind::Stretch(1))
    );
}

/// Stretch 5 fits per frame: a moving one refuses with its note.
#[test]
fn a_moving_stretch_5_refuses() {
    assert_eq!(
        refusal(stretched(5, WIDE, TALL)),
        (CriticalPart::Cover, NoteKind::Stretch(5))
    );
    // A static one still imports.
    let held = cover_image(&format!(r#""stretch": 5, {}"#, still(TALL)));
    assert!(held.result.is_ok());
}

/// Keys of one branch each bake the stretch.
#[test]
fn a_stretch_within_one_branch_bakes_into_each_key() {
    let l = stretched(1, WIDE, WIDER);
    assert_eq!(d3_notes(&l), 1);
    let pose = |p: &str| cover_image(&format!(r#""stretch": 1, {}"#, still(p)));
    for (ms, p) in [(0, WIDE), (249, WIDE_MID), (500, WIDER), (5_000, WIDER)] {
        assert_eq!(rects(&l, ms), rects(&pose(p), ms), "{ms}");
    }
    assert_ne!(rects(&l, 0), rects(&l, 500));
}

/// The cover's slide and a follower's motion stay live as the entrance
/// plays.
#[test]
fn a_moving_pose_still_follows_the_covers() {
    let at_cover = |l: &Loaded, ms, cover| {
        let mut d = Drawn::new(l.ok().skin.clone(), 0);
        d.view.cover = cover;
        d.shown(ms)
    };
    let paint = cover_image(&moving(500));
    for (ms, pose) in [(250, MID), (1_000, B)] {
        let lowered = at_cover(&paint, ms, 500);
        assert_ne!(lowered, at_cover(&paint, ms, 0));
        assert_eq!(lowered, at_cover(&cover_image(&still(pose)), ms, 500));
    }
    // A follower needs the lanes.
    let over_lanes = |track: &str| {
        let parts = r#"{"id": "n0", "src": "a", "w": 8, "h": 8},
                       {"id": "n1", "src": "a", "w": 8, "h": 8},
                       {"id": "n2", "src": "a", "w": 8, "h": 8},
                       {"id": "n3", "src": "a", "w": 8, "h": 8}"#;
        let objects = format!(
            r#"{}, "gauge": {{"id": "g", "nodes": ["n0", "n1", "n2", "n3"], "parts": 10}}"#,
            note("")
        );
        let d = format!(r#"{{"id": "g", "offset": 4, {track}}}"#);
        import_body(&body(parts, &objects, &[NOTES, &d]))
    };
    let follower = over_lanes(&moving(500));
    assert_eq!(d3_notes(&follower), 1);
    for (ms, pose) in [(250, MID), (1_000, B)] {
        assert_ne!(at_cover(&follower, ms, 500), at_cover(&follower, ms, 0));
        let posed = at_cover(&over_lanes(&still(pose)), ms, 500);
        assert_eq!(at_cover(&follower, ms, 500), posed, "{ms}");
    }
}

/// A number following HIDDEN+ (offset 5) with `keys`, over the lanes.
fn hidden_follower(keys: &str) -> Loaded {
    let number = r#""value": [{"id": "num", "src": "a", "w": 60, "h": 6, "divx": 10, "digit": 1, "ref": 71}]"#;
    let objects = format!("{}, {number}", note(""));
    let d = format!(r#"{{"id": "num", "offsets": [5], "loop": 500, "dst": [{keys}]}}"#);
    import_body(&body("", &objects, &[NOTES, &d]))
}

/// A follower of HIDDEN+ is hidden by an alpha offset the source skips
/// while the colour interpolates (`SkinObject.java:490-519`): a settling
/// one whose colour changes refuses with the offset's note; moving with a
/// fixed colour, it plays.
#[test]
fn a_hidden_follower_s_changing_colour_refuses() {
    let tinted = r#"{"time": 0, "x": 0, "y": 0, "w": 10, "h": 10},
                    {"time": 500, "x": 0, "y": 40, "w": 10, "h": 10, "r": 0}"#;
    assert_eq!(
        refusal(hidden_follower(tinted)),
        (CriticalPart::ExScore, NoteKind::OffsetAlpha)
    );
    let moved = r#"{"time": 0, "x": 0, "y": 0, "w": 10, "h": 10},
                   {"time": 500, "x": 0, "y": 40, "w": 10, "h": 10}"#;
    assert_eq!(d3_notes(&hidden_follower(moved)), 1);
}

#[test]
fn conditions_stay_on_a_settling_track() {
    let track = format!(r#""op": [241], {}"#, moving(500));
    let l = number(&track);
    assert_eq!(d3_notes(&l), 1);
    let mut d = Drawn::new(l.ok().skin.clone(), 100);
    assert_eq!(d.shown(250), Vec::<[i32; 4]>::new());
    assert_eq!(rects(&l, 249), rects(&number(&still(MID)), 249));
}

#[test]
fn draw_order_stays() {
    let around = |track: &str| {
        import_body(&format!(
            r#""image": [{{"id": "i", "src": "a", "w": 8, "h": 8}}],
               "value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 1,
                           "ref": 71}}],
               "destination": [{{"id": "i", "dst": [{{"x": 1, "y": 2, "w": 3, "h": 4}}]}},
                               {{"id": "v", {track}}},
                               {{"id": "i", "dst": [{{"x": 5, "y": 6, "w": 7, "h": 8}}]}}]"#
        ))
    };
    let settling = around(&moving(500));
    assert_eq!(d3_notes(&settling), 1);
    for (ms, pose) in [(249, MID), (500, B)] {
        let shown = rects(&settling, ms);
        assert_eq!(shown.len(), 3);
        assert_eq!(shown, rects(&around(&still(pose)), ms), "{ms}");
    }
}

/// One D3 note per destination, whatever else it carries: settling and
/// an untimed cycle on one destination give one.
#[test]
fn one_note_per_destination_beside_d3() {
    let dsts = format!(r#"{{"id": "g", {t}}}, {{"id": "g", {t}}}"#, t = moving(500));
    let l = import_body(&format!(r#"{GAUGE}, "destination": [{dsts}]"#));
    // Each stands at its own destination, where the gauge's D10 is.
    let d10: Vec<_> = l
        .report()
        .notes
        .iter()
        .filter(|n| n.kind == NoteKind::Deviation(DeviationId::D10))
        .map(|n| n.at)
        .collect();
    assert_eq!(d3_at(&l), d10);
    assert_eq!(d3_at(&l).len(), 2);
    assert_ne!(d3_at(&l)[0], d3_at(&l)[1]);
    assert_eq!(rects(&l, 249), rects(&gauge(&still(MID)), 249).repeat(2));

    let cycling = |track: &str| {
        import_body(&format!(
            r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 20, "digit": 4,
                           "ref": 71, "cycle": 100}}],
               "destination": [{{"id": "v", {track}}}]"#
        ))
    };
    let l = cycling(&moving(500));
    assert_eq!(d3_notes(&l), 1);
    assert_eq!(rects(&l, 249), rects(&cycling(&still(MID)), 249));
}

/// A judge's outer destination draws no values of its own, so nothing
/// settles there: its keys stay and it is not noted.
#[test]
fn a_neutral_destination_never_settles() {
    let grade = r#"{"id": "g0", "dst": [{"x": 100, "y": 500, "w": 80, "h": 20}]}"#;
    let l = import_body(&format!(
        r#""image": [{{"id": "g0", "src": "a", "w": 8, "h": 8}}],
           "judge": [{{"id": "j", "index": 0, "images": [{grade}], "numbers": []}}],
           "destination": [{{"id": "j", {}}}]"#,
        moving(500)
    ));
    assert_eq!(d3_notes(&l), 0);
}

/// Decorative elements animate as before: no note, no refusal, the
/// motion kept.
#[test]
fn decorative_elements_are_unchanged() {
    let image = |track: &str| {
        import_body(&format!(
            r#""image": [{{"id": "i", "src": "a", "w": 8, "h": 8}}],
               "destination": [{{"id": "i", {track}}}]"#
        ))
    };
    for looped in [500, 200, 0, -1] {
        assert_eq!(d3_notes(&image(&moving(looped))), 0, "loop {looped}");
    }
    let l = image(&moving(500));
    let mut d = Drawn::new(l.ok().skin.clone(), 0);
    assert_eq!(d.shown(0), [[20, -320, 430, 600]]);
    let mut d = Drawn::new(l.ok().skin.clone(), 0);
    assert_eq!(d.shown(500), [[20, -120, 430, 500]]);

    // HIDDEN+ is decorative too.
    let track = hidden(r#""loop": 500"#).replace(
        r#""dst": [{"x": 20, "y": -400, "w": 430, "h": 600}]"#,
        r#""dst": [{"time": 0, "x": 20, "y": -400, "w": 430, "h": 600},
                   {"time": 500, "x": 20, "y": -300, "w": 430, "h": 500}]"#,
    );
    assert_eq!(d3_notes(&with(&hidden_object(""), &[&track])), 0);
}

/// Self-made in the shape of a real skin's entrance: a gauge growing from
/// no height after a hold, eased, under a position offset and a lowering
/// alpha offset with a fixed colour; and a SUDDEN+ slider sliding down
/// after a hold. Both settle, pass the gate and every later check, and
/// play their keys with one D3 note each.
#[test]
fn rm_shaped_gauge_and_cover_are_admitted() {
    let skin = |gauge: &str, cover: &str| {
        let objects = format!(
            r#""offset": [{{"name": "p", "id": 41, "x": true, "y": true, "w": true, "h": true}},
                          {{"name": "t", "id": 42, "a": true}}],
               {}, "gauge": {{"id": "g", "nodes": ["g0", "g1", "g2", "g3"], "parts": 10}},
               "slider": [{{"id": "sl", "src": "a", "x": 0, "y": 24, "w": 16, "h": 16,
                            "type": 4, "angle": 2, "range": 600}}]"#,
            note("")
        );
        let g = format!(r#"{{"id": "g", "offsets": [41, 42], {gauge}}}"#);
        let c = format!(r#"{{"id": "sl", {cover}}}"#);
        import_config(
            [1_920, 1_080],
            &body("", &objects, &[NOTES, &g, &c]),
            &[],
            |c: &mut Config| {
                c.offsets.insert(
                    41,
                    OffsetValue {
                        x: 5,
                        ..OffsetValue::default()
                    },
                );
                c.offsets.insert(
                    42,
                    OffsetValue {
                        a: -51,
                        ..OffsetValue::default()
                    },
                );
            },
        )
    };
    let gauge_keys = r#""loop": 500, "dst": [
        {"time": 0, "x": 470, "y": 100, "w": 300, "h": 0, "a": 200, "acc": 2},
        {"time": 300}, {"time": 500, "h": 24}]"#;
    let cover_keys = r#""loop": 400, "dst": [
        {"time": 0, "x": 20, "y": 1400, "w": 430, "h": 600},
        {"time": 100}, {"time": 400, "y": 800}]"#;
    let gauge_at =
        |h: i32| format!(r#""dst": [{{"x": 470, "y": 100, "w": 300, "h": {h}, "a": 200}}]"#);
    let cover_at = |y: i32| format!(r#""dst": [{{"x": 20, "y": {y}, "w": 430, "h": 600}}]"#);
    let l = skin(gauge_keys, cover_keys);
    let kinds: Vec<_> = l.report().notes.iter().map(|n| n.kind).collect();
    for kind in [NoteKind::Timer(TimerWhy::Untimed), NoteKind::OffsetAlpha] {
        assert!(!kinds.contains(&kind), "{kinds:?}");
    }
    assert!(
        !kinds.iter().any(|k| matches!(k, NoteKind::Stretch(_))),
        "{kinds:?}"
    );
    let d3 = d3_at(&l);
    assert_eq!(d3.len(), 2, "{kinds:?}");
    assert_ne!(d3[0], d3[1]);
    // The gauge's alpha is 200 − 51 at every frame it shows: its ten
    // parts, each 30 wide.
    for ms in [400, 5_000] {
        let a: Vec<f32> = Drawn::new(l.ok().skin.clone(), 100)
            .frame(ms, &[])
            .iter()
            .filter(|q| q.rect[3] > 0.0 && (q.rect[2] - 30.0).abs() < 0.5)
            .map(|q| q.color[3])
            .collect();
        assert_eq!(a, [149.0 / 255.0; 10], "{ms}");
    }
    // At their keys, the poses authored alone; between, the cover on its
    // way down.
    for (ms, h, y) in [
        (0, 0, 1_400),
        (99, 0, 1_400),
        (500, 24, 800),
        (5_000, 24, 800),
    ] {
        let posed = skin(&gauge_at(h), &cover_at(y));
        assert_eq!(rects(&l, ms), rects(&posed, ms), "{ms}");
    }
    let mid = rects(&l, 249);
    assert_ne!(mid, rects(&skin(&gauge_at(0), &cover_at(1_400)), 249));
    assert_ne!(mid, rects(&skin(&gauge_at(0), &cover_at(800)), 249));
    assert_eq!(mid, rects(&skin(&gauge_at(0), &cover_at(1_100)), 249));
}
