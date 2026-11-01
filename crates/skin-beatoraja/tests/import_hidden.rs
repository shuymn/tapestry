//! HIDDEN+ (`hiddenCover`): the subset that maps, its trim line, offsets
//! and switch, what is dropped with a note, and the differences noted
//! against the source, through `build`.
//!
//! The default lanes (r = 1, y 200, h 600, size 20): top 270, bottom 880,
//! a 600 px lane over x [20, 450]. The default HIDDEN+ is 600 tall with
//! its upper edge on the lanes' bottom (880, y down), trimmed there as
//! the lift raises it.

mod common;

use bms_model::{ChartBuilder, Lane, TimeUs};
use common::lanes::{
    NOTES, body, cover, hidden, hidden_object, lane_rects, lift, lift_object, note, note_with, with,
};
use common::play::{Drawn, MS};
use common::{Loaded, has, import_body, import_config};
use skin_beatoraja::{
    Config, DeviationId, LaneApprox, NoteKind, OffsetValue, PropertyKind, Severity,
};

/// The default HIDDEN+ with `extra` object fields and `dst` extra fields.
fn hid(extra: &str, dst: &str) -> Loaded {
    with(&hidden_object(extra), &[&hidden(dst)])
}

/// The quads at 1,000 ms with no note on screen and the covers at
/// `[cover, lift, hidden]` ‰.
fn quads(l: &Loaded, [cover, lift, hidden]: [u16; 3]) -> Vec<skin_model::Quad> {
    let chart = ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(5_000 * MS))
        .build()
        .unwrap();
    let mut d = Drawn::over(l.ok().skin.clone(), &chart);
    (d.view.cover, d.view.lift, d.view.hidden) = (cover, lift, hidden);
    d.frame(1_000, &[])
}

/// [`quads`]' rects, rounded.
fn rects(l: &Loaded, view: [u16; 3]) -> Vec<[i32; 4]> {
    quads(l, view)
        .iter()
        .map(|q| common::play::round(q.rect))
        .collect()
}

fn d9(a: LaneApprox) -> NoteKind {
    NoteKind::Deviation(DeviationId::D9(a))
}

/// Whether `l` imported with `kind` dropped and draws no HIDDEN+.
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
    assert_eq!(rects(l, [0, 0, 1_000]), Vec::<[i32; 4]>::new(), "{kind:?}");
}

#[test]
fn hidden_plus_rises_from_its_destination() {
    let l = hid("", "");
    let r = l.report();
    for a in [
        LaneApprox::HiddenOrder,
        LaneApprox::HiddenStretch,
        LaneApprox::HiddenClip,
        LaneApprox::CoverOrder,
    ] {
        assert!(!has(r, d9(a)), "{a:?}");
    }
    // Nothing at 0 ‰. 500 ‰ raises it 300 px: its upper edge at 580, cut
    // at the lanes' lower edge. At 1 000 ‰ it covers the whole region.
    assert_eq!(rects(&l, [0, 0, 0]), Vec::<[i32; 4]>::new());
    assert_eq!(rects(&l, [0, 0, 500]), [[20, 580, 430, 300]]);
    assert_eq!(rects(&l, [0, 0, 1_000]), [[20, 280, 430, 600]]);
    // LIFT 200 ‰ (120 px) raises it and its linked trim; 500 ‰ of the
    // 480 px left raises it 240 more: from 520 down to the raised 760.
    assert_eq!(rects(&l, [0, 200, 500]), [[20, 520, 430, 240]]);
    // Its own image, cached apart from every other one.
    assert_eq!(
        r.usage.images,
        with("", &[]).report().usage.images + 1,
        "one image more"
    );
}

#[test]
fn the_trim_line_is_fixed_linked_or_absent() {
    // Unlinked, the trim stays at 880 under the raised line.
    let fixed = hid(r#""isDisapearLineLinkLift": false"#, "");
    assert_eq!(rects(&fixed, [0, 200, 500]), [[20, 520, 430, 360]]);
    // A full lift leaves it no share, but it stays on: drawn after the
    // judgment line, from the lanes' top (280) down to the fixed trim,
    // and not at all under a linked one, raised to its upper edge.
    let line = rects(&fixed, [0, 1_000, 0]);
    assert_eq!(line.len(), 1);
    let mut want = line.clone();
    want.push([20, 280, 430, 600]);
    assert_eq!(rects(&fixed, [0, 1_000, 500]), want);
    assert_eq!(rects(&hid("", ""), [0, 1_000, 500]), line);
    // 100 px higher, at rest 780: 100 ‰ raises it 60 to 720. A line at
    // 300 trims it at 780, linked or not; none cuts it at the lanes.
    let up = |obj: &str| {
        with(
            &hidden_object(obj),
            &[&hidden("").replace(r#""y": -400"#, r#""y": -300"#)],
        )
    };
    // (The last of a repeated field wins.)
    let at300 = up(r#""disapearLine": 300"#);
    assert_eq!(rects(&at300, [0, 0, 100]), [[20, 720, 430, 60]]);
    let unlinked = up(r#""disapearLine": 300, "isDisapearLineLinkLift": false"#);
    assert_eq!(rects(&unlinked, [0, 0, 100]), [[20, 720, 430, 60]]);
    let none = with(
        &hidden_object("").replace(r#""disapearLine": 200"#, r#""disapearLine": -1"#),
        &[&hidden("").replace(r#""y": -400"#, r#""y": -300"#)],
    );
    assert_eq!(rects(&none, [0, 0, 100]), [[20, 720, 430, 160]]);
    // Without the field there is no trim either: the default -1.
    let absent = with(
        &hidden_object("").replace(r#", "disapearLine": 200"#, ""),
        &[&hidden("").replace(r#""y": -400"#, r#""y": -300"#)],
    );
    assert_eq!(rects(&absent, [0, 0, 100]), [[20, 720, 430, 160]]);
    // Untrimmed, the source draws it below the lanes; risen 600 px from
    // 780, above them.
    for l in [&none, &absent, &at300] {
        assert!(has(l.report(), d9(LaneApprox::HiddenClip)));
    }
    // From its default place untrimmed, only below them.
    let below = with(
        &hidden_object("").replace(r#""disapearLine": 200"#, r#""disapearLine": -1"#),
        &[&hidden("")],
    );
    assert!(has(below.report(), d9(LaneApprox::HiddenClip)));
    // 100 tall from 800, it rises at most to 200, never above a line at
    // 980 (100), fixed or linked: the source never draws it, so nothing
    // is drawn outside the lanes.
    for link in ["false", "true"] {
        let obj = hidden_object(&format!(r#""isDisapearLineLinkLift": {link}"#))
            .replace(r#""disapearLine": 200"#, r#""disapearLine": 980"#);
        let dst = hidden("").replace(
            r#""y": -400, "w": 430, "h": 600"#,
            r#""y": 180, "w": 430, "h": 100"#,
        );
        let l = with(&obj, &[&dst]);
        assert!(!has(l.report(), d9(LaneApprox::HiddenClip)), "{link}");
        for lift in [0, 300, 1_000] {
            assert_eq!(
                rects(&l, [0, lift, 1_000]),
                rects(&l, [0, lift, 0]),
                "{link} {lift}"
            );
        }
    }
}

#[test]
fn a_raised_trim_above_its_upper_edge_draws_nothing() {
    // The source region's lower edge at 140, 580 high; HIDDEN+ hangs from
    // it, its trim linked at 357. LIFT 300 ‰ (174) and HIDDEN+ 500 ‰ (203)
    // raise its upper edge to 563, under the trim raised to 549.
    let notes = note_with(&lane_rects(140, 580), "");
    let objects = format!(
        "{notes}, {}",
        hidden_object("").replace(r#""disapearLine": 200"#, r#""disapearLine": 357"#)
    );
    let dst = r#"{"id": "hid", "dst": [{"x": 20, "y": -440, "w": 430, "h": 580}]}"#;
    let l = import_body(&body("", &objects, &[NOTES, dst]));
    assert_eq!(rects(&l, [0, 300, 500]), Vec::<[i32; 4]>::new());
    // At 600 ‰ (243.6) it rises to 522.4, above the trim.
    let q = quads(&l, [0, 300, 600]);
    let upper = (940.0_f64 - 174.0 - 243.6) as f32;
    assert_eq!(
        q.iter().map(|q| q.rect).collect::<Vec<_>>(),
        [[20.0, upper, 430.0, 549.0 - upper]]
    );
}

#[test]
fn fractional_edges_are_kept() {
    // A 1280×720 skin scales by 1.5: the lanes' lower edge at 780 and 900
    // high, HIDDEN+ 900 tall from 780, and a fixed line at 201 trims at
    // 1 080 − 301.5 = 778.5. 500 ‰ raises it 450 to 330.
    let objects = format!(
        "{}, {}",
        note(""),
        hidden_object(r#""isDisapearLineLinkLift": false"#)
            .replace(r#""disapearLine": 200"#, r#""disapearLine": 201"#)
    );
    let l = import_config(
        [1_280, 720],
        &body("", &objects, &[NOTES, &hidden("")]),
        &[],
        |_| {},
    );
    let q = quads(&l, [0, 0, 500]);
    assert_eq!(
        q.iter().map(|q| q.rect).collect::<Vec<_>>(),
        [[30.0, 330.0, 645.0, 448.5]]
    );
    // Linked, LIFT 100 ‰ (90) raises the trim to 688.5 and the cover by
    // 90 and 405: to 285.
    let objects = format!(
        "{}, {}",
        note(""),
        hidden_object("").replace(r#""disapearLine": 200"#, r#""disapearLine": 201"#)
    );
    let l = import_config(
        [1_280, 720],
        &body("", &objects, &[NOTES, &hidden("")]),
        &[],
        |_| {},
    );
    let q = quads(&l, [0, 100, 500]);
    assert_eq!(
        q.iter().map(|q| q.rect).collect::<Vec<_>>(),
        [[30.0, 285.0, 645.0, 403.5]]
    );
}

#[test]
fn fit_width_hidden_plus_is_trimmed_after_its_stretch() {
    // The 16×16 cell fit to 430 is 430 high, centred on the 600 px
    // destination: its upper edge at 965. 500 ‰ raises it to 665, cut at
    // 880. The source decides its trim against the unstretched
    // destination: noted.
    for mode in [4, 5] {
        let l = hid("", &format!(r#""stretch": {mode}"#));
        assert_eq!(rects(&l, [0, 0, 500]), [[20, 665, 430, 215]], "{mode}");
        assert!(has(l.report(), d9(LaneApprox::HiddenStretch)), "{mode}");
    }
    // Without a trim line nothing is decided against it: not noted.
    let none = hidden_object("").replace(r#""disapearLine": 200"#, r#""disapearLine": -1"#);
    let l = with(&none, &[&hidden(r#""stretch": 4"#)]);
    assert!(!has(l.report(), d9(LaneApprox::HiddenStretch)));
    // Enlarged, it rises above where the source decides: a 43×10 cell
    // fit to 430 is 100 high, centred on a 10 px destination at rest at
    // [800, 810]. 500 ‰ raises them to [455, 555] and [500, 510]; a fixed
    // line at 590 (490) leaves the unstretched edge below it, so the
    // source draws nothing, and the model draws the stretched one down to
    // the line.
    let cell = |link: bool, line: i32| {
        hidden_object(&format!(r#""isDisapearLineLinkLift": {link}"#))
            .replace(
                r#""x": 32, "y": 24, "w": 16, "h": 16"#,
                r#""x": 0, "y": 0, "w": 43, "h": 10"#,
            )
            .replace(
                r#""disapearLine": 200"#,
                &format!(r#""disapearLine": {line}"#),
            )
    };
    let small = hidden(r#""stretch": 4"#).replace(
        r#""y": -400, "w": 430, "h": 600"#,
        r#""y": 270, "w": 430, "h": 10"#,
    );
    let whole = quads(&with(&cell(false, -1), &[&small]), [0, 0, 500]);
    let [w] = &whole[..] else {
        panic!("one HIDDEN+: {whole:?}");
    };
    assert_eq!(w.rect, [20.0, 455.0, 430.0, 100.0]);
    let objects = format!("{}, {}", note(""), cell(false, 590));
    let text = body("", &objects, &[NOTES, &small]);
    let l = import_body(&text);
    let cut = quads(&l, [0, 0, 500]);
    let [q] = &cut[..] else {
        panic!("one HIDDEN+: {cut:?}");
    };
    let [u0, v0, u1, v1] = w.uv;
    assert_eq!(q.rect, [20.0, 455.0, 430.0, 35.0]);
    assert_eq!(q.uv, [u0, v0, u1, v0 + (v1 - v0) * 0.35]);
    // Noted at its destination, which the body starts on the skin's
    // second line.
    let found: Vec<_> = l
        .report()
        .notes
        .iter()
        .filter(|n| n.kind == d9(LaneApprox::HiddenStretch))
        .map(|n| n.at.map(|at| (at.line, at.col)))
        .collect();
    let before = &text[..text.find(r#"{"id": "hid", "stretch""#).unwrap()];
    let line = 2 + before.matches('\n').count();
    let col = before.rsplit('\n').next().unwrap().len() + 1;
    let at = [line, col].map(|v| u32::try_from(v).unwrap());
    assert_eq!(found, [Some((at[0], at[1]))]);
    // The source decides whether to draw it from the unstretched edge,
    // which must pass strictly above the line. Risen by the lane's 600 px,
    // the 10 px destination reaches 200: a line at 890 (190), fixed or
    // linked, keeps it from ever drawing, though the stretched edge
    // reaches 155. A destination without height at 800 also reaches 200:
    // a line at 880 (200) or 881 (199) keeps it from drawing. A line at
    // 879 (201) lets either draw above the lanes, the flat one's stretched
    // [750, 850] at rest.
    let flat = hidden(r#""stretch": 4"#).replace(
        r#""y": -400, "w": 430, "h": 600"#,
        r#""y": 280, "w": 430, "h": 0"#,
    );
    for link in [false, true] {
        for (h, dst, line, outside) in [
            (10, &small, 890, false),
            (10, &small, 879, true),
            (0, &flat, 880, false),
            (0, &flat, 881, false),
            (0, &flat, 879, true),
        ] {
            let r = with(&cell(link, line), &[dst]);
            let case = format!("h {h}, line {line}, linked {link}");
            assert!(has(r.report(), d9(LaneApprox::HiddenStretch)), "{case}");
            assert_eq!(
                has(r.report(), d9(LaneApprox::HiddenClip)),
                outside,
                "{case}"
            );
        }
    }
    // Once the unstretched lower edge is at or above the trim, the source
    // draws the stretched one whole, past the trim: a 64×1 cell fit to 430
    // is 6.72 high, centred on [876, 880] at [874.64, 881.36], so under a
    // line at 200 (880), even at 1 ‰ (0.6 px), it reaches below the lanes.
    let thin = hidden_object("").replace(
        r#""x": 32, "y": 24, "w": 16, "h": 16"#,
        r#""x": 0, "y": 0, "w": 64, "h": 1"#,
    );
    let low = hidden(r#""stretch": 4"#).replace(
        r#""y": -400, "w": 430, "h": 600"#,
        r#""y": 200, "w": 430, "h": 4"#,
    );
    assert!(has(
        with(&thin, &[&low]).report(),
        d9(LaneApprox::HiddenClip)
    ));
    // A 16×32 cell fit to 430 would be 860 high: stretch 5 keeps the
    // destination and crops the cell's middle rows, as SUDDEN+.
    let tall = hidden_object("").replace(r#""h": 16"#, r#""h": 32"#);
    let l = with(&tall, &[&hidden(r#""stretch": 5"#)]);
    assert_eq!(rects(&l, [0, 0, 500]), [[20, 580, 430, 300]]);
    assert!(!has(l.report(), d9(LaneApprox::HiddenStretch)));
    // A grid cannot crop: dropped.
    let grid = hidden_object("").replace(
        r#""x": 32, "y": 24, "w": 16, "h": 16"#,
        r#""x": 32, "y": 0, "w": 16, "h": 64, "divy": 2"#,
    );
    dropped(
        &with(&grid, &[&hidden(r#""stretch": 5"#)]),
        NoteKind::Stretch(5),
    );
}

#[test]
fn hidden_plus_shares_its_file_with_sudden_plus() {
    // SUDDEN+ draws `cover` of `a.png`; HIDDEN+ its own cell of it.
    let alone = with("", &[&cover("")]);
    let both = with(&hidden_object(""), &[&cover(""), &hidden("")]);
    let (a, b) = (alone.report().usage, both.report().usage);
    assert_eq!((b.images, b.files), (a.images + 1, a.files));
}

#[test]
fn its_switch_is_the_amount() {
    // A positive op 273, by number or name, or `draw` 273: HIDDEN+ is
    // drawn exactly while its amount is above 0.
    for extra in [
        r#""op": [273]"#,
        r#""op": [273, 273]"#,
        r#""op": ["hidden1_on"]"#,
        r#""op": ["!!hidden1_on"]"#,
        r#""draw": 273"#,
        r#""draw": "hidden1_on""#,
    ] {
        let l = hid("", extra);
        assert_eq!(rects(&l, [0, 0, 0]), Vec::<[i32; 4]>::new(), "{extra}");
        assert_eq!(rects(&l, [0, 0, 500]), [[20, 580, 430, 300]], "{extra}");
    }
    // Its negation is unsupported, as the global switch.
    let neg = |id| NoteKind::UnsupportedProperty {
        kind: PropertyKind::Boolean,
        id: Some(id),
    };
    dropped(&hid("", r#""op": [-273]"#), neg(-273));
    dropped(&hid("", r#""op": ["!hidden1_on"]"#), neg(-273));
}

#[test]
fn lift_and_hidden_offsets_are_its_geometry() {
    // Offsets 3 and 5, however listed, are what the model moves it by.
    for extra in [
        r#""offset": 5"#,
        r#""offset": 3"#,
        r#""offsets": [3, 5, 5], "offset": 3"#,
        r#""offsets": [5]"#,
    ] {
        let l = hid("", extra);
        assert_eq!(rects(&l, [0, 200, 500]), [[20, 520, 430, 240]], "{extra}");
    }
    for id in [1, 2, 4] {
        dropped(
            &hid("", &format!(r#""offset": {id}"#)),
            NoteKind::RuntimeOffset(id),
        );
    }
}

#[test]
fn hidden_plus_outside_the_subset_is_dropped() {
    let dst = NoteKind::UnsupportedField { field: "dst" };
    let at = |rect: &str| hidden("").replace(r#""x": 20, "y": -400, "w": 430, "h": 600"#, rect);
    for d in [
        // Not across the lanes, without height, or of negative height.
        at(r#""x": 20, "y": -400, "w": 215, "h": 600"#),
        at(r#""x": 20, "y": -400, "w": 430, "h": 0"#),
        at(r#""x": 20, "y": -400, "w": 430, "h": -1"#),
        // Rotated, coloured, translucent or animated.
        at(r#""x": 20, "y": -400, "w": 430, "h": 600, "angle": 10"#),
        at(r#""x": 20, "y": -400, "w": 430, "h": 600, "r": 0"#),
        at(r#""x": 20, "y": -400, "w": 430, "h": 600, "a": 200"#),
        hidden(r#""loop": -1"#).replace(
            r#""dst": [{"x": 20, "y": -400, "w": 430, "h": 600}]"#,
            r#""dst": [{"time": 0, "x": 20, "y": -400, "w": 430, "h": 600, "a": 0},
                       {"time": 1600, "x": 20, "y": -400, "w": 430, "h": 600}]"#,
        ),
        // Its own timer, conditions or blend.
        hidden(r#""timer": 41"#),
        hidden(r#""op": [240]"#),
        hidden(r#""op": [273, 240]"#),
        hidden(r#""blend": 2"#),
    ] {
        dropped(&with(&hidden_object(""), &[&d]), dst);
    }
    dropped(&hid("", r#""stretch": 1"#), NoteKind::Stretch(1));
    // A negative height, even where a stretch would give it one.
    let negative = hidden(r#""stretch": 4"#).replace(r#""h": 600"#, r#""h": -1"#);
    dropped(&with(&hidden_object(""), &[&negative]), dst);
    // Before the notes, or a second one.
    let early = import_body(&body(
        "",
        &format!("{}, {}", note(""), hidden_object("")),
        &[&hidden(""), NOTES],
    ));
    dropped(
        &early,
        NoteKind::UnsupportedField {
            field: "hiddenCover",
        },
    );
    let twice = with(&hidden_object(""), &[&hidden(""), &hidden("")]);
    assert!(has(
        twice.report(),
        NoteKind::UnsupportedField {
            field: "hiddenCover"
        }
    ));
    assert_eq!(rects(&twice, [0, 0, 500]), [[20, 580, 430, 300]]);
    // A missing file.
    let missing = hidden_object("").replace(r#""src": "a""#, r#""src": "nothing""#);
    dropped(&with(&missing, &[&hidden("")]), NoteKind::MissingAsset);
    // A user offset moving it.
    let header = r#""offset": [{"name": "y", "id": 40, "y": true}]"#;
    let objects = format!("{header}, {}, {}", note(""), hidden_object(""));
    let moved = import_config(
        [1_920, 1_080],
        &body("", &objects, &[NOTES, &hidden(r#""offsets": [40]"#)]),
        &[],
        |c: &mut Config| {
            c.offsets.insert(
                40,
                OffsetValue {
                    y: 1,
                    ..OffsetValue::default()
                },
            );
        },
    );
    dropped(&moved, dst);
}

#[test]
fn hidden_plus_is_drawn_under_the_other_covers() {
    // After SUDDEN+ or LIFT in the source's order, the source draws
    // HIDDEN+ above it; the model always draws it below: noted.
    let order = d9(LaneApprox::HiddenOrder);
    let objects = format!("{}, {}", lift_object(""), hidden_object(""));
    for (dsts, noted) in [
        (vec![cover(""), hidden("")], true),
        (vec![lift(""), hidden("")], true),
        (vec![hidden(""), cover(""), lift("")], false),
    ] {
        let dsts: Vec<&str> = dsts.iter().map(String::as_str).collect();
        let l = with(&objects, &dsts);
        assert_eq!(has(l.report(), order), noted, "{dsts:?}");
    }
    // Notes, LIFT, then HIDDEN+ under a fixed trim: they overlap from
    // 760, and LIFT is drawn above.
    let fixed = format!(
        "{}, {}",
        lift_object(""),
        hidden_object(r#""isDisapearLineLinkLift": false"#)
    );
    let l = with(&fixed, &[&lift(""), &hidden("")]);
    assert!(has(l.report(), order));
    assert_eq!(
        rects(&l, [0, 200, 500]),
        [[20, 520, 430, 360], [20, 760, 430, 120]]
    );
    // An element kept between the notes and HIDDEN+ is drawn above it.
    let plain = r#"{"id": "deco", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}"#;
    let l = with(&hidden_object(""), &[plain, &hidden("")]);
    assert!(has(l.report(), d9(LaneApprox::CoverOrder)));
    assert!(!has(l.report(), order));
}
