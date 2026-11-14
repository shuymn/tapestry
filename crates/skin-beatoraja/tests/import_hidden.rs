//! LIFT's (`liftCover`) and HIDDEN+'s (`hiddenCover`) paint: their
//! motion and implicit offsets, the trim line and the unstretched
//! destination that gates and clips them, HIDDEN+'s switch, what is
//! dropped with a note, and the differences noted against the source,
//! through `build`.
//!
//! The default lanes (r = 1, y 200, h 600, size 20): top 270, bottom 880,
//! a 600 px region over x [20, 450]. The default HIDDEN+ is 600 tall with
//! its upper edge on the lanes' bottom (880, y down), trimmed there as
//! the lift raises it; the default LIFT is 200 tall below the lanes,
//! trimmed at their bottom.

mod common;

use bms_model::ChartBuilder;
use common::lanes::{NOTES, body, hidden, hidden_object, lift, lift_object, note, with};
use common::play::{Drawn, readouts_for};
use common::{Loaded, has, import_body, import_sized, kinds};
use skin_beatoraja::{
    Config, CriticalPart, DeviationId, ImportError, LaneApprox, NoteKind, OffsetValue, Severity,
};

/// What shows at 1,000 ms with the covers at `[cover, lift, hidden]` ‰
/// and HIDDEN+ `enabled`: every quad cut to its clip.
fn shown_with(l: &Loaded, [cover, lift, hidden]: [u16; 3], enabled: bool) -> Vec<[i32; 4]> {
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let mut d = Drawn::over(l.ok().skin.clone(), &chart);
    (d.view.cover, d.view.lift, d.view.hidden) = (cover, lift, if enabled { hidden } else { 0 });
    let mut readouts = readouts_for(d.view);
    (readouts.covers.hidden, readouts.covers.hidden_enabled) = (hidden, enabled);
    d.readouts = Some(readouts);
    d.shown(1_000)
}

/// [`shown_with`] HIDDEN+ on.
fn shown(l: &Loaded, view: [u16; 3]) -> Vec<[i32; 4]> {
    shown_with(l, view, true)
}

/// The quads with their clips at 1,000 ms, every cover at rest.
fn clipped(l: &Loaded) -> Vec<([i32; 4], Option<[i32; 4]>)> {
    let chart = ChartBuilder::new(150_000).build().unwrap();
    Drawn::over(l.ok().skin.clone(), &chart).clipped(1_000)
}

fn d9(a: LaneApprox) -> NoteKind {
    NoteKind::Deviation(DeviationId::D9(a))
}

/// HIDDEN+ with `extra` object fields and `dst` extra fields.
fn hid(extra: &str, dst: &str) -> Loaded {
    with(&hidden_object(extra), &[&hidden(dst)])
}

#[test]
fn the_paint_rises_with_its_covers() {
    // H = 600: lift 300 ‰ is 180 px and HIDDEN+ 500 ‰ of the 420 left is
    // 210: LIFT's paint rises 180, cut at its fixed line 880; HIDDEN+'s
    // rises 390, cut at its linked line, 880 raised to 700.
    let objects = format!("{}, {}", hidden_object(""), lift_object(""));
    let l = with(&objects, &[&hidden(""), &lift("")]);
    assert_eq!(
        shown(&l, [0, 300, 500]),
        [[20, 490, 430, 210], [20, 700, 430, 180]]
    );
    // At rest, each lies at or below its line: nothing draws.
    assert!(shown(&l, [0, 0, 0]).is_empty());
    // The implicit offsets, listed again in any form, apply once.
    let listed = hidden(r#""offset": 3, "offsets": [5, 3, 5]"#);
    let lifted = lift(r#""offsets": [3, 3]"#);
    let l = with(&objects, &[&listed, &lifted]);
    assert_eq!(
        shown(&l, [0, 300, 500]),
        [[20, 490, 430, 210], [20, 700, 430, 180]]
    );
    assert!(!has(l.report(), NoteKind::RuntimeOffset(3)));
}

#[test]
fn offset_four_adds_the_cover_once() {
    // Lift 300 ‰ (180 px), the cover 500 ‰ of the 420 left (210 px):
    // an untrimmed LIFT's paint listing offset 4, twice, moves 30 down.
    let untrimmed = lift_object("").replace("200", "-1");
    let l = with(&untrimmed, &[&lift(r#""offsets": [4, 4]"#)]);
    assert_eq!(shown(&l, [500, 300, 0]), [[20, 910, 430, 200]]);
    let motion = d9(LaneApprox::CoverMotion);
    assert_eq!(
        kinds(l.report()).iter().filter(|&&k| k == motion).count(),
        1
    );
}

#[test]
fn the_trim_line_is_fixed_linked_or_absent() {
    // HIDDEN+ at 1 000 ‰ over the default lanes at lift 200 ‰ (120 px):
    // it rises 120 + 480; linked, its line rises to 760; fixed, it stays
    // at 880; a negative line, or none, does not trim.
    let view = [0, 200, 1_000];
    assert_eq!(shown(&hid("", ""), view), [[20, 280, 430, 480]]);
    let fixed = hid(r#""isDisapearLineLinkLift": false"#, "");
    assert_eq!(shown(&fixed, view), [[20, 280, 430, 600]]);
    for line in ["-1", "-200"] {
        let l = with(&hidden_object("").replace("200", line), &[&hidden("")]);
        assert_eq!(shown(&l, view), [[20, 280, 430, 600]], "{line}");
    }
    let unlined = hidden_object("").replace(
        r#""disapearLine": 200"#,
        r#""isDisapearLineLinkLift": true"#,
    );
    assert_eq!(
        shown(&with(&unlined, &[&hidden("")]), view),
        [[20, 280, 430, 600]]
    );
    // LIFT's line is fixed unless linked.
    let linked = lift_object(r#""isDisapearLineLinkLift": true"#);
    assert!(shown(&with(&linked, &[&lift("")]), [0, 200, 0]).is_empty());
}

#[test]
fn the_trim_line_rounds_half_up() {
    // r = 3/2: line 133 is 199.5 skin pixels up, 880.5 down, rounded to
    // 881; LIFT's 300 px from 780 is cut 101 down.
    let objects = format!("{}, {}", note(""), lift_object("").replace("200", "133"));
    let l = import_sized([1_280, 720], &body("", &objects, &[NOTES, &lift("")]), &[]);
    assert_eq!(
        clipped(&l),
        [([30, 780, 645, 300], Some([30, 780, 645, 101]))]
    );
}

/// A 32×32 cell of `a.png` as `id`, trimmed at `line`, drawn by `dst`
/// with `stretch`, without lanes.
fn gated(id: &str, line: i32, stretch: i32, dst: [i32; 4]) -> Loaded {
    let [x, y, w, h] = dst;
    let object = format!(
        r#""{id}": [{{"id": "c", "src": "a", "x": 0, "y": 0, "w": 32, "h": 32, "disapearLine": {line}}}]"#
    );
    let d = format!(
        r#"{{"id": "c", "stretch": {stretch}, "dst": [{{"x": {x}, "y": {y}, "w": {w}, "h": {h}}}]}}"#
    );
    import_body(&body("", &object, &[&d]))
}

#[test]
fn an_empty_gate_crossed_by_the_line_draws_nothing() {
    // Stretch 9 keeps the 32×32 frame over the empty [0, 100, 0, 10]
    // (y down): at 105 the part above the line has no area; at 120 the
    // destination is wholly above it and the paint is whole.
    for id in ["liftCover", "hiddenCover"] {
        assert!(
            clipped(&gated(id, 1_080 - 105, 9, [0, 970, 0, 10])).is_empty(),
            "{id}"
        );
        assert_eq!(
            clipped(&gated(id, 1_080 - 120, 9, [0, 970, 0, 10])),
            [([-16, 89, 32, 32], None)],
            "{id}"
        );
    }
}

#[test]
fn the_unstretched_destination_gates_a_fitted_paint() {
    // Stretch 4 fits the 32×32 frame to 100 px: [100, 110] (y down)
    // becomes [55, 155]. Line 90: nothing, though the fit reaches above
    // it; line 120: whole, reaching below it; line 105: cut to the
    // destination above it.
    let dst = [0, 970, 100, 10];
    for id in ["liftCover", "hiddenCover"] {
        assert!(clipped(&gated(id, 1_080 - 90, 4, dst)).is_empty(), "{id}");
        assert_eq!(
            clipped(&gated(id, 1_080 - 120, 4, dst)),
            [([0, 55, 100, 100], None)],
            "{id}"
        );
        assert_eq!(
            clipped(&gated(id, 1_080 - 105, 4, dst)),
            [([0, 55, 100, 100], Some([0, 100, 100, 5]))],
            "{id}"
        );
    }
    // The gate's keys are charged besides the paint's; an unstretched or
    // untrimmed paint has none.
    let keys = |line: i32, stretch: i32| gated("liftCover", line, stretch, dst).report().usage.keys;
    assert_eq!(keys(975, 4), keys(-1, 4) + 1);
    assert_eq!(keys(975, 0), keys(-1, 0));
    assert_eq!(keys(975, 9), keys(-1, 9));
}

#[test]
fn hidden_paint_shows_only_while_hidden_is_on() {
    // On at 0 ‰, the paint stands where authored; off at any amount,
    // nothing.
    let untrimmed = hidden_object("").replace("200", "-1");
    let l = with(&untrimmed, &[&hidden("")]);
    assert_eq!(shown(&l, [0, 0, 0]), [[20, 880, 430, 600]]);
    for amount in [0, 500] {
        assert!(shown_with(&l, [0, 0, amount], false).is_empty(), "{amount}");
    }
    // Its switch, op 273, is an ordinary condition: compiled, either way.
    let ops = |l: &Loaded| l.report().usage.ops;
    let on = with(&untrimmed, &[&hidden(r#""op": [273]"#)]);
    assert!(ops(&on) > ops(&l));
    assert_eq!(shown(&on, [0, 0, 0]), [[20, 880, 430, 600]]);
    let negated = with(&untrimmed, &[&hidden(r#""op": [-273]"#)]);
    assert!(ops(&negated) > ops(&l));
    assert!(shown(&negated, [0, 0, 0]).is_empty());
    // Animated colour is noted: the source can skip the off alpha while
    // the colour interpolates.
    assert!(!has(l.report(), d9(LaneApprox::HiddenDisable)));
    let fading = r#"{"id": "hid", "dst": [
        {"time": 0, "x": 20, "y": -400, "w": 430, "h": 600, "a": 255},
        {"time": 1000, "a": 0}]}"#;
    let l = with(&untrimmed, &[fading]);
    assert!(has(l.report(), d9(LaneApprox::HiddenDisable)));
}

#[test]
fn every_destination_paints_in_its_place() {
    // Two HIDDEN+ destinations, before the notes and after an image, and
    // HIDDEN+ without lanes, at its authored rect.
    let untrimmed = hidden_object("").replace("200", "-1");
    let plain = r#"{"id": "deco", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}"#;
    let l = import_body(&body(
        "",
        &format!("{}, {untrimmed}", note("")),
        &[&hidden(""), NOTES, plain, &hidden("")],
    ));
    assert_eq!(
        shown(&l, [0, 0, 0]),
        [[20, 880, 430, 600], [0, 1_070, 10, 10], [20, 880, 430, 600]]
    );
    let l = import_body(&body("", &untrimmed, &[&hidden("")]));
    assert_eq!(shown(&l, [0, 0, 0]), [[20, 880, 430, 600]]);
}

/// Whether `l` imported with `kind` dropped.
#[track_caller]
fn dropped(l: &Loaded, kind: NoteKind) {
    let r = l.report();
    assert!(
        r.notes
            .iter()
            .any(|n| n.severity == Severity::Dropped && n.kind == kind),
        "{kind:?}: {:?}",
        r.notes
    );
}

#[test]
fn unsupported_hidden_plus_is_dropped() {
    // A trimming stretch.
    dropped(&hid("", r#""stretch": 3"#), NoteKind::Stretch(3));
    // A trimmed destination of negative height.
    let flipped = hidden("").replace(r#""h": 600"#, r#""h": -600"#);
    dropped(
        &with(&hidden_object(""), &[&flipped]),
        NoteKind::UnsupportedField { field: "dst" },
    );
    // A configured offset 5 moving it other than up.
    let header = r#""offset": [{"name": "h", "id": 5, "x": true}]"#;
    let objects = format!("{header}, {}, {}", note(""), hidden_object(""));
    let l = common::import_config(
        [1_920, 1_080],
        &body("", &objects, &[NOTES, &hidden("")]),
        &[],
        |c: &mut Config| {
            c.offsets.insert(
                5,
                OffsetValue {
                    x: 3,
                    ..OffsetValue::default()
                },
            );
        },
    );
    dropped(&l, NoteKind::RuntimeOffset(5));
}

#[test]
fn a_negative_size_rounding_to_zero_is_still_refused() {
    // At 3840×2160 (r = 1/2) a width of −1 is −0.5 reference pixels and
    // its edges round to one x; a height of −1 from y 1941 likewise. The
    // line 1920 is at 120, below either rect [0, 100, 0, 10] or
    // [0, 110, 10, 0]: an unchecked stretch-9 paint would draw whole.
    for key in [
        r#"{"x": 0, "y": 1940, "w": -1, "h": 20}"#,
        r#"{"x": 0, "y": 1941, "w": 20, "h": -1}"#,
    ] {
        let at = |object: &str, id: &str| {
            let objects = format!("{}, {}", note(""), object.replace("200", "1920"));
            let dst = format!(r#"{{"id": "{id}", "stretch": 9, "dst": [{key}]}}"#);
            import_sized([3_840, 2_160], &body("", &objects, &[NOTES, &dst]), &[])
        };
        dropped(
            &at(&hidden_object(""), "hid"),
            NoteKind::UnsupportedField { field: "dst" },
        );
        match at(&lift_object(""), "liftc").err() {
            ImportError::Critical { part, why, .. } => {
                assert_eq!(part, CriticalPart::Lift, "{key}");
                assert_eq!(why, NoteKind::UnsupportedField { field: "dst" }, "{key}");
            }
            e => panic!("{key}: {e:?}"),
        }
    }
}

/// What shows at 1,000 ms with the covers at `[cover, lift, hidden]` ‰,
/// HIDDEN+ `enabled` and the adjustment control `held` (op 270).
fn shown_held(l: &Loaded, view: [u16; 3], enabled: bool, held: bool) -> Vec<[i32; 4]> {
    let [cover, lift, hidden] = view;
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let mut d = Drawn::over(l.ok().skin.clone(), &chart);
    (d.view.cover, d.view.lift, d.view.hidden) = (cover, lift, if enabled { hidden } else { 0 });
    let mut readouts = readouts_for(d.view);
    (readouts.covers.hidden, readouts.covers.hidden_enabled) = (hidden, enabled);
    readouts.covers.adjusting = held;
    d.readouts = Some(readouts);
    d.shown(1_000)
}

/// A one-digit number off the lanes listing `offsets`, under `op`.
fn amount(offsets: &str, op: &str) -> Loaded {
    let number = r#""value": [{"id": "num", "src": "a", "w": 60, "h": 6, "divx": 10, "digit": 1, "ref": 71}]"#;
    let objects = format!("{}, {number}", note(""));
    let num = format!(
        r#"{{"id": "num", "offsets": [{offsets}], "op": [{op}], "dst": [{{"x": 0, "y": 0, "w": 10, "h": 10}}]}}"#
    );
    import_body(&body("", &objects, &[NOTES, &num]))
}

/// `rects` moved down by `dy`.
fn moved(rects: &[[i32; 4]], dy: i32) -> Vec<[i32; 4]> {
    rects
        .iter()
        .map(|&[x, y, w, h]| [x, y + dy, w, h])
        .collect()
}

#[test]
fn a_number_listing_offset_five_follows_hidden_plus() {
    // The HIDDEN+ amount, shown while the adjustment control is held and
    // HIDDEN+ is on: HIDDEN+ at 300 ‰ of the 600 px lane raises it 180,
    // the source's `hidden × lane height` (`play/LaneRenderer.java:
    // 335-346`).
    let l = amount("5", "270, 273");
    assert!(!has(l.report(), NoteKind::RuntimeOffset(5)));
    assert!(has(l.report(), d9(LaneApprox::CoverMotion)));
    let rest = shown_held(&l, [0, 0, 0], true, true);
    assert_eq!(rest.len(), 1);
    assert_eq!(shown_held(&l, [0, 0, 300], true, true), moved(&rest, -180));
    // Not held, it does not show.
    assert!(shown_held(&l, [0, 0, 300], true, false).is_empty());
}

#[test]
fn a_follower_of_hidden_plus_does_not_draw_while_it_is_off() {
    // Without its switch in the condition, the follower alone hides it,
    // as the source's alpha offset of −255 does.
    let l = amount("5", "");
    let on = shown_held(&l, [0, 0, 0], true, false);
    assert_eq!(on.len(), 1);
    assert!(shown_held(&l, [0, 0, 300], false, false).is_empty());
    assert!(shown_held(&l, [0, 0, 0], false, false).is_empty());
    // An element that does not follow HIDDEN+ still draws.
    let l = amount("4", "");
    assert_eq!(shown_held(&l, [0, 0, 300], false, false).len(), 1);
}

#[test]
fn offsets_three_four_and_five_add() {
    // LIFT 100 ‰ of 600 is 60 up; SUDDEN+ 500 ‰ of the 540 left is 270
    // down; HIDDEN+ 300 ‰ of the 540 is 162 up: 48 down together, and
    // each subset its own sum.
    let rest = shown_held(&amount("3, 4, 5", ""), [0, 0, 0], true, false);
    assert_eq!(rest.len(), 1);
    for (offsets, dy) in [
        ("3, 4, 5", 48),
        ("3", -60),
        ("4", 270),
        ("5", -162),
        ("3, 4", 210),
        ("3, 5", -222),
        ("4, 5", 108),
    ] {
        let l = amount(offsets, "");
        assert_eq!(
            shown_held(&l, [500, 100, 300], true, false),
            moved(&rest, dy),
            "{offsets}"
        );
    }
}

#[test]
fn a_follower_of_hidden_plus_needs_a_fixed_colour_and_no_raised_alpha() {
    // The source skips every alpha offset at an interpolated colour, so a
    // fading follower would show while HIDDEN+ is off
    // (`skin/SkinObject.java:489-504`): refused, as any alpha offset on
    // animated colour is.
    let number = r#""value": [{"id": "num", "src": "a", "w": 60, "h": 6, "divx": 10, "digit": 1, "ref": 71}]"#;
    let objects = format!(
        r#""offset": [{{"name": "a", "id": 42, "a": true}}], {}, {number}"#,
        note("")
    );
    let num = |fields: &str, keys: &str| {
        format!(r#"{{"id": "num", "offsets": [{fields}], "dst": [{keys}]}}"#)
    };
    let fading =
        r#"{"time": 0, "x": 0, "y": 0, "w": 10, "h": 10, "a": 255}, {"time": 1000, "a": 0}"#;
    let still = r#"{"x": 0, "y": 0, "w": 10, "h": 10}"#;
    let load = |d: &str, a: i32| {
        common::import_config(
            [1_920, 1_080],
            &body("", &objects, &[NOTES, d]),
            &[],
            |c: &mut Config| {
                c.offsets.insert(
                    42,
                    OffsetValue {
                        a,
                        ..OffsetValue::default()
                    },
                );
            },
        )
    };
    // The number shows the EX score, a critical part: refused.
    let why = |l: Loaded| match l.err() {
        ImportError::Critical { why, .. } => why,
        e => panic!("{e:?}"),
    };
    assert_eq!(why(load(&num("5", fading), 0)), NoteKind::OffsetAlpha);
    // Without offset 5 the fade imports.
    assert!(!has(
        load(&num("4", fading), 0).report(),
        NoteKind::OffsetAlpha
    ));
    // A raised alpha after offset 5 clamps it back up in the source,
    // 255 → 0 → 128 while HIDDEN+ is off: refused.
    assert_eq!(why(load(&num("5, 42", still), 128)), NoteKind::OffsetAlpha);
    // A lowered one keeps one sign: admitted and folded.
    let l = load(&num("5, 42", still), -128);
    assert!(!has(l.report(), NoteKind::OffsetAlpha));
    assert_eq!(shown_held(&l, [0, 0, 0], true, false).len(), 1);
    assert!(shown_held(&l, [0, 0, 0], false, false).is_empty());
}

/// Mixed alpha deltas on a follower of HIDDEN+ still refuse when one is
/// above 0, though their sum is below: +64 and −128.
#[test]
fn a_follower_of_hidden_plus_refuses_mixed_alpha_raising_it() {
    let number = r#""value": [{"id": "num", "src": "a", "w": 60, "h": 6, "divx": 10, "digit": 1, "ref": 71}]"#;
    let objects = format!(
        r#""offset": [{{"name": "a", "id": 42, "a": true}}, {{"name": "b", "id": 43, "a": true}}], {}, {number}"#,
        note("")
    );
    let num =
        r#"{"id": "num", "offsets": [5, 42, 43], "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}"#;
    let l = common::import_config(
        [1_920, 1_080],
        &body("", &objects, &[NOTES, num]),
        &[],
        |c: &mut Config| {
            for (id, a) in [(42, 64), (43, -128)] {
                c.offsets.insert(
                    id,
                    OffsetValue {
                        a,
                        ..OffsetValue::default()
                    },
                );
            }
        },
    );
    match l.err() {
        ImportError::Critical { why, .. } => assert_eq!(why, NoteKind::OffsetAlpha),
        e => panic!("{e:?}"),
    }
}
