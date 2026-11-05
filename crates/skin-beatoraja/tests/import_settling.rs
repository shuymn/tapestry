//! Critical untimed tracks that settle (D20): each element kind that
//! reaches the rule, held at its last key from the start, against the
//! same destination authored as that one key, and what still refuses.

mod common;

use common::lanes::{NOTES, body, hidden, hidden_object, lift_object, note, with};
use common::play::{Drawn, judged};
use common::{Loaded, d3_notes, import_body, import_config};
use rules::Judgment;
use skin_beatoraja::{
    Config, CriticalPart, DeviationId, ImportError, NoteKind, OffsetValue, Severity, TimerWhy,
    TrackWhy,
};

/// The first key.
const A: &str = r#""x": 20, "y": 800, "w": 430, "h": 600"#;
/// The last key: moved and resized.
const B: &str = r#""x": 20, "y": 700, "w": 430, "h": 500"#;

/// `A` at 0 and `B` at 500 under `loop`.
fn moving(looped: i32) -> String {
    format!(r#""loop": {looped}, "dst": [{{"time": 0, {A}}}, {{"time": 500, {B}}}]"#)
}

/// `B` alone: what a settling track must show.
fn still() -> String {
    format!(r#""dst": [{{{B}}}]"#)
}

fn d20(l: &Loaded) -> usize {
    let kind = NoteKind::Deviation(DeviationId::D20);
    l.report().notes.iter().filter(|n| n.kind == kind).count()
}

/// The rects shown at `ms`, just after a judgment, with SUDDEN+ at 500 ‰.
fn rects(l: &Loaded, ms: i64) -> Vec<[i32; 4]> {
    let mut d = Drawn::new(l.ok().skin.clone(), 100);
    d.view.cover = 500;
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

/// Shows `settling` matching `still`, from before its entrance would have
/// ended until long after, with one located D20 note and no D3.
#[track_caller]
fn assert_held(settling: &Loaded, still: &Loaded, case: &str) {
    assert_eq!(d20(settling), 1, "{case}");
    assert_eq!(d3_notes(settling), 0, "{case}");
    assert_eq!(d20(still), 0, "{case}");
    let note = settling
        .report()
        .notes
        .iter()
        .find(|n| n.kind == NoteKind::Deviation(DeviationId::D20));
    assert_eq!(
        note.map(|n| n.severity),
        Some(Severity::Deviation),
        "{case}"
    );
    for ms in [0, 250, 501, 5_000] {
        let held = rects(settling, ms);
        assert!(!held.is_empty(), "{case} at {ms}");
        assert_eq!(held, rects(still, ms), "{case} at {ms}");
    }
}

/// A track settles at `loop` on its last key, and past it where its first
/// key shows the last's values; a first key after 0 is lost with the rest.
fn settling(kind: fn(&str) -> Loaded) {
    assert_held(&kind(&moving(500)), &kind(&still()), "loop at the last key");
    let back = format!(
        r#""loop": 900, "dst": [{{"time": 0, {B}}}, {{"time": 250, {A}}}, {{"time": 500, {B}}}]"#
    );
    assert_held(&kind(&back), &kind(&still()), "loop past the last key");
    let late = format!(r#""loop": 500, "dst": [{{"time": 100, {A}}}, {{"time": 500, {B}}}]"#);
    assert_held(&kind(&late), &kind(&still()), "a first key after 0");
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

/// On a timer of its own a track plays as authored.
fn timed(kind: fn(&str) -> Loaded) {
    let l = kind(&format!(r#""timer": 41, {}"#, moving(500)));
    assert_eq!((d20(&l), d3_notes(&l)), (0, 0));
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
            fn settling_holds_its_last_key() {
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

#[test]
fn a_colour_channel_settles_as_a_move_does() {
    let tinted =
        format!(r#""loop": 100, "dst": [{{"time": 0, {B}}}, {{"time": 100, {B}, "r": 0}}]"#);
    let l = number(&tinted);
    assert_eq!((d20(&l), d3_notes(&l)), (1, 0));
    let red: Vec<f32> = number(&tinted)
        .quads(0)
        .iter()
        .map(|q| q.color[0])
        .collect();
    assert_eq!(red, [0.0]);
    let looping = tinted.replace(r#""loop": 100"#, r#""loop": 50"#);
    assert_eq!(
        refusal(number(&looping)),
        (CriticalPart::ExScore, NoteKind::Timer(TimerWhy::Untimed))
    );
}

/// An alpha in a moving track is held with its pose; a track changing only
/// its alpha is not touched and keeps its fade on `screen` (D3).
#[test]
fn alpha_is_held_with_the_pose_and_a_fade_alone_stays_d3() {
    let track = format!(
        r#""loop": 500, "dst": [{{"time": 0, {A}, "a": 0}}, {{"time": 500, {B}, "a": 102}}]"#
    );
    assert_eq!(d20(&gauge(&track)), 1);
    let alphas =
        |t: &str, ms| -> Vec<f32> { gauge(t).quads(ms).iter().map(|q| q.color[3]).collect() };
    assert_eq!(alphas(&track, 0), [0.4; 10]);
    assert_eq!(alphas(&track, 250), [0.4; 10]);

    let fade = format!(
        r#""loop": 500, "dst": [{{"time": 0, {B}, "a": 0}}, {{"time": 500, {B}, "a": 255}}]"#
    );
    let l = gauge(&fade);
    assert_eq!((d20(&l), d3_notes(&l)), (0, 1));
    assert_eq!(alphas(&fade, 250), [0.5; 10]);
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

#[test]
fn an_offset_applies_to_the_key_it_is_held_at() {
    let v = OffsetValue {
        x: 7,
        a: -102,
        ..OffsetValue::default()
    };
    let settled = offset_gauge(&moving(500), v);
    assert_held(&settled, &offset_gauge(&still(), v), "offset");
    assert_ne!(
        rects(&settled, 0),
        rects(&offset_gauge(&still(), OffsetValue::default()), 0)
    );
    let alpha: Vec<f32> = settled.quads(0).iter().map(|q| q.color[3]).collect();
    assert_eq!(alpha, [0.6; 10]);

    // An alpha offset on a colour that changes still refuses.
    let fading = format!(
        r#""loop": 500, "dst": [{{"time": 0, {A}, "a": 0}}, {{"time": 500, {B}, "a": 255}}]"#
    );
    assert_eq!(
        refusal(offset_gauge(&fading, v)),
        (CriticalPart::Gauge, NoteKind::OffsetAlpha)
    );
}

/// Stretch is judged on the last key alone, whatever branch the others
/// would have taken, and stretch 5 sees a static track.
#[test]
fn a_stretch_bakes_into_the_key_it_is_held_at() {
    let wide = r#""x": 20, "y": 800, "w": 600, "h": 300"#;
    let tall = r#""x": 20, "y": 800, "w": 300, "h": 600"#;
    for mode in [1, 5] {
        let track = format!(
            r#""stretch": {mode}, "loop": 500, "dst": [{{"time": 0, {wide}}},
                                                     {{"time": 500, {tall}}}]"#
        );
        let held = format!(r#""stretch": {mode}, "dst": [{{{tall}}}]"#);
        let (settled, still) = (cover_image(&track), cover_image(&held));
        assert_held(&settled, &still, "stretch");
    }
}

/// The cover's slide and a follower's motion stay live on the held pose.
#[test]
fn a_held_pose_still_follows_the_covers() {
    let at_cover = |l: &Loaded, cover| {
        let mut d = Drawn::new(l.ok().skin.clone(), 0);
        d.view.cover = cover;
        d.shown(1_000)
    };
    let paint = cover_image(&moving(500));
    let lowered = at_cover(&paint, 500);
    assert_ne!(lowered, at_cover(&paint, 0));
    assert_eq!(lowered, at_cover(&cover_image(&still()), 500));
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
    let (follower, still_follower) = (over_lanes(&moving(500)), over_lanes(&still()));
    assert_eq!(d20(&follower), 1);
    assert_ne!(at_cover(&follower, 500), at_cover(&follower, 0));
    assert_eq!(at_cover(&follower, 500), at_cover(&still_follower, 500));
}

#[test]
fn conditions_stay_on_a_held_pose() {
    let track = format!(r#""op": [241], {}"#, moving(500));
    let l = number(&track);
    assert_eq!(d20(&l), 1);
    let mut d = Drawn::new(l.ok().skin.clone(), 100);
    assert_eq!(d.shown(1_000), Vec::<[i32; 4]>::new());
    assert!(!rects(&l, 1_000).is_empty());
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
    let (settled, plain) = (around(&moving(500)), around(&still()));
    assert_eq!(d20(&settled), 1);
    let shown = rects(&settled, 0);
    assert_eq!(shown.len(), 3);
    assert_eq!(shown, rects(&plain, 0));
}

/// One note per destination, whatever else it carries, and an untimed
/// cycle stays D3 beside it.
#[test]
fn one_note_per_destination_beside_d3() {
    assert_eq!(d20(&gauge(&moving(500))), 1);
    let dsts = format!(r#"{{"id": "g", {t}}}, {{"id": "g", {t}}}"#, t = moving(500));
    let l = import_body(&format!(r#"{GAUGE}, "destination": [{dsts}]"#));
    assert_eq!(d20(&l), 2);

    let cycling = import_body(&format!(
        r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 20, "digit": 4,
                       "ref": 71, "cycle": 100}}],
           "destination": [{{"id": "v", {}}}]"#,
        moving(500)
    ));
    assert_eq!((d20(&cycling), d3_notes(&cycling)), (1, 1));
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
    assert_eq!((d20(&l), d3_notes(&l)), (0, 0));
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
        let l = image(&moving(looped));
        assert_eq!((d20(&l), d3_notes(&l)), (0, 0), "loop {looped}");
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
    let l = with(&hidden_object(""), &[&track]);
    assert_eq!(d20(&l), 0);
}
