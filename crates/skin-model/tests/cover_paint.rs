//! Cover paint apart from the logical coverage, on the source player's
//! lanes (`lanes_support::source`: the region 600 high from 280 to 880,
//! the lanes' top at 270) at 1920×1080 with the author's layout: offset
//! and slider motion, the coverage extremes, paint that never changes the
//! notes, whole authored rects, the draw order of every cover's paint,
//! independent clocks, choices, the load checks, the capacity and the
//! variant. The trim lines are `cover_trim.rs`'s.

mod lanes_support;
mod play_support;

use bms_model::{Chart, ChartBuilder, TimeUs};
use lanes_support::{
    COVER, HIDDEN, LIFT, NOTE, WHITE, cover_image, image, in_lane, lanes_element, moving, paint,
    quads_of, rects, sheet, simple, source,
};
use play_support::{Harness, MS, REGIONS, bool_program, full, lane, readouts_for};
use skin_model::{
    Blend, ClipId, CoverImage, CoverMotion, CoverPaintDef, CoverPaintError, CoverTrim, CycleDef,
    ElementDef, Fit, Follow, ImageChoiceDef, ImageChoicePart, ImageSpan, LANE_QUADS, LANE_RESERVED,
    LaneView, LanesDef, Layout, LoadError, MAX_COORD, MAX_ELEMENTS, Native, PLAY, Place, Scope,
    Skin, SkinDef, TimerRef, Ty, Variant, WidgetDefs, compile, timer,
};

/// 150 BPM, in thousandths.
const BPM: i64 = 150_000;

fn def(lanes: Option<LanesDef>, elements: Vec<ElementDef>, widgets: WidgetDefs) -> SkinDef {
    SkinDef {
        fit: Fit::Adapt,
        regions: REGIONS,
        whens: Vec::new(),
        sheet: sheet(),
        elements,
        lanes,
        widgets,
        bomb_up_to: Some(skin_model::Grade::Bad),
    }
}

fn paints(defs: Vec<CoverPaintDef>) -> WidgetDefs {
    WidgetDefs {
        cover_paints: defs,
        ..WidgetDefs::default()
    }
}

/// The source lanes with `elements` after them and the paints `defs`.
fn skin(elements: Vec<ElementDef>, defs: Vec<CoverPaintDef>) -> Skin {
    let mut all = vec![lanes_element()];
    all.extend(elements);
    Skin::new(def(Some(source()), all, paints(defs))).unwrap()
}

fn tap_chart() -> Chart {
    ChartBuilder::new(BPM)
        .tap(lane(3), TimeUs(1_000 * MS))
        .tap(lane(3), TimeUs(1_300 * MS))
        .build()
        .unwrap()
}

/// A harness on `skin` showing `[cover, lift, hidden]` at 700 ms.
fn shown(skin: Skin, [cover, lift, hidden]: [u16; 3]) -> Harness {
    let mut h = Harness::new(skin, &tap_chart(), &full(0));
    (h.view.cover, h.view.lift, h.view.hidden) = (cover, lift, hidden);
    h.frame(700, &[]);
    h
}

/// An offset paint (`+C`), a slider of 800 and a slider of 800 that also
/// follows the cover, each its own row of [`COVER`].
fn motion_skin() -> Skin {
    skin(
        vec![
            paint(0, [160, -320, 544, 600], Follow::Cover),
            paint(1, [160, 0, 544, 100], Follow::None),
            paint(1, [160, 0, 544, 100], Follow::Cover),
        ],
        vec![cover_image(None), cover_image(Some(800))],
    )
}

fn ys(h: &Harness) -> Vec<f32> {
    rects(h, COVER).iter().map(|r| r[1]).collect()
}

#[test]
fn offsets_and_sliders_add_their_motion() {
    // H = 600; lift 250 ‰ leaves 450, cover 500 ‰ of it is C = 225. A
    // slider of 800 moves 800 · 0.5 · 0.75 = 300, and one listing offset 4
    // as well moves both, 525.
    let h = shown(motion_skin(), [500, 250, 0]);
    assert_eq!(ys(&h), [-95.0, 300.0, 525.0]);
    // 800 · 501 · 749 / 10⁶ = 300.1992, not a floored readout's 300; the
    // offset's C is 501 ‰ of 449.4.
    let h = shown(motion_skin(), [501, 251, 0]);
    let c = 600.0 * 0.749 * 0.501;
    let want = |base: f64, dy: f64| (base + dy) as f32;
    assert_eq!(
        ys(&h),
        [
            want(-320.0, c),
            want(0.0, 300.1992),
            want(0.0, 300.1992 + c)
        ]
    );
    assert_ne!(ys(&h)[1], 300.0);
    // At rest nothing moves.
    let h = shown(motion_skin(), [0, 0, 0]);
    assert_eq!(ys(&h), [-320.0, 0.0, 0.0]);
}

#[test]
fn switched_off_covers_move_by_their_effective_amounts() {
    // The cover on at 500 ‰, the lift off with 300 ‰ stored: the view's
    // lift is 0, so C = 300 and the slider moves 800 · 0.5 = 400.
    let mut h = Harness::new(motion_skin(), &tap_chart(), &full(0));
    h.view.cover = 500;
    let mut readouts = readouts_for(h.view);
    (readouts.covers.lift, readouts.covers.lift_enabled) = (300, false);
    h.readouts = Some(readouts);
    h.frame(700, &[]);
    assert_eq!(ys(&h), [-20.0, 400.0, 700.0]);
    // The cover off with 500 ‰ stored: nothing moves, and the paint,
    // governed by its own visibility only, still draws at rest.
    h.view.cover = 0;
    let mut readouts = readouts_for(h.view);
    (readouts.covers.cover, readouts.covers.cover_enabled) = (500, false);
    h.readouts = Some(readouts);
    h.frame(700, &[]);
    assert_eq!(ys(&h), [-320.0, 0.0, 0.0]);
}

#[test]
fn coverage_extremes_keep_the_authored_paint() {
    // At 0 and 1 000 ‰, also with no scroll left, each paint keeps its
    // size and its whole image, and nothing else is painted.
    let whole = |h: &Harness| h.uvs.get(COVER).unwrap().uv;
    for (view, want) in [
        ([0, 0, 0], [-320.0, 0.0, 0.0]),
        ([1_000, 0, 0], [280.0, 800.0, 1_400.0]),
        ([1_000, 1_000, 0], [-320.0, 0.0, 0.0]),
        ([0, 1_000, 1_000], [-320.0, 0.0, 0.0]),
    ] {
        let h = shown(motion_skin(), view);
        let q = quads_of(&h, COVER);
        assert_eq!(ys(&h), want, "{view:?}");
        assert!(
            q.iter().all(
                |q| q.rect[3] == if q.rect[1] == want[0] { 600.0 } else { 100.0 }
                    && q.uv == whole(&h)
                    && q.rect.iter().all(|v| v.is_finite())
            ),
            "{view:?}"
        );
    }
}

#[test]
fn paint_never_changes_the_notes() {
    // Whatever the paint shows, or whether it shows, the notes scroll
    // under the same coverage: none, transparent, hidden, its timer off,
    // a hole or a negative selection.
    let notes = |elements: Vec<ElementDef>, defs: Vec<CoverPaintDef>, choices| {
        let mut all = vec![lanes_element()];
        all.extend(elements);
        let widgets = WidgetDefs {
            cover_paints: defs,
            image_choices: choices,
            ..WidgetDefs::default()
        };
        let skin = Skin::new(def(Some(source()), all, widgets)).unwrap();
        let h = shown(skin, [300, 100, 0]);
        (in_lane(&h, NOTE, lane(3)), rects(&h, COVER).len())
    };
    let base = notes(Vec::new(), Vec::new(), Vec::new());
    assert_eq!(base.0.len(), 2);
    let p = || paint(0, [160, -320, 544, 600], Follow::Cover);
    let mut clear = p();
    let mut props = [None; 9];
    props[..5].copy_from_slice(&[Some(160), Some(-320), Some(544), Some(600), Some(0)]);
    clear.track = lanes_support::still(props);
    let one = || vec![cover_image(Some(200))];
    let choice = |value: &str| {
        vec![ImageChoiceDef {
            value: compile(value, &Scope::new(&PLAY), Ty::Int).unwrap(),
            choices: vec![None],
        }]
    };
    let by_choice = || {
        vec![CoverPaintDef {
            image: CoverImage::Choice(0),
            slider_range: Some(200),
            ..cover_image(None)
        }]
    };
    for (what, elements, defs, choices, drawn) in [
        ("transparent", vec![clear], one(), Vec::new(), 1),
        (
            "hidden",
            vec![ElementDef {
                visible: Some(bool_program("false")),
                ..p()
            }],
            one(),
            Vec::new(),
            0,
        ),
        (
            "timer off",
            vec![ElementDef {
                timer: TimerRef::Builtin(timer::FULL_COMBO),
                ..p()
            }],
            one(),
            Vec::new(),
            0,
        ),
        ("a hole", vec![p()], by_choice(), choice("0"), 0),
        ("negative", vec![p()], by_choice(), choice("-1"), 0),
    ] {
        let got = notes(elements, defs, choices);
        assert_eq!(got.0, base.0, "{what}");
        assert_eq!(got.1, drawn, "{what}");
    }
}

#[test]
fn paint_draws_its_whole_authored_rects() {
    // Split, wider than the lanes and wholly above them: each rect as
    // authored, its whole image, no lane cut; an explicit clip stays.
    let clipped = ElementDef {
        clip: Some(ClipId(1)),
        ..paint(0, [160, -200, 544, 50], Follow::None)
    };
    let h = shown(
        skin(
            vec![
                paint(0, [160, -100, 272, 500], Follow::None),
                paint(0, [432, -100, 272, 500], Follow::None),
                paint(0, [100, -50, 700, 80], Follow::None),
                clipped,
            ],
            vec![cover_image(None)],
        ),
        [500, 0, 0],
    );
    assert_eq!(
        rects(&h, COVER),
        [
            [160.0, -100.0, 272.0, 500.0],
            [432.0, -100.0, 272.0, 500.0],
            [100.0, -50.0, 700.0, 80.0],
            [160.0, -200.0, 544.0, 50.0],
        ]
    );
    let whole = h.uvs.get(COVER).unwrap().uv;
    assert!(quads_of(&h, COVER).iter().all(|q| q.uv == whole));
    let last = h.batch.runs().last().unwrap();
    assert_eq!(last.key.clip, Some(ClipId(1)));
}

/// The parts `h` drew among `parts`, in draw order.
fn order(h: &Harness, parts: &[u32]) -> Vec<u32> {
    h.batch
        .quads()
        .iter()
        .filter_map(|q| {
            parts.iter().copied().find(|&p| {
                let f = h.uvs.get(p).unwrap();
                f.layer == q.layer && [f.uv[0], f.uv[2]] == [q.uv[0], q.uv[2]]
            })
        })
        .collect()
}

/// SUDDEN+'s paint at 0, then LIFT's (1) over `[160, 880, 544, 200]`
/// trimmed at the region's bottom, and HIDDEN+'s (2) over the region,
/// trimmed there as it rises with the lift.
fn three_paints() -> Vec<CoverPaintDef> {
    vec![
        cover_image(None),
        moving(LIFT, CoverMotion::Lift, CoverTrim::Fixed(880)),
        moving(HIDDEN, CoverMotion::Hidden, CoverTrim::Lift(880)),
    ]
}

fn lift_paint() -> ElementDef {
    paint(1, [160, 880, 544, 200], Follow::None)
}

fn hidden_paint() -> ElementDef {
    paint(2, [160, 280, 544, 600], Follow::None)
}

#[test]
fn every_cover_paint_draws_in_list_order() {
    // SUDDEN+, an additive image, LIFT, SUDDEN+, HIDDEN+, an additive
    // SUDDEN+: drawn in that order after the lanes. Lift 100 ‰ (L = 60)
    // and HIDDEN+ 500 ‰ (K = 270) cut LIFT at 880, a run of its own
    // clip, and leave HIDDEN+ (−50 to 550) wholly above the raised line
    // (820), so it shares the plain alpha run before it.
    let add = |e: ElementDef| ElementDef {
        blend: Blend::Add,
        ..e
    };
    let elements = vec![
        lanes_element(),
        paint(0, [160, -320, 544, 600], Follow::Cover),
        add(image(WHITE, [0, 0, 10, 10], Place::SCREEN)),
        lift_paint(),
        paint(0, [160, 0, 544, 100], Follow::None),
        hidden_paint(),
        add(paint(0, [160, 100, 544, 100], Follow::None)),
    ];
    let skin = Skin::new(def(Some(source()), elements, paints(three_paints()))).unwrap();
    let h = shown(skin, [200, 100, 500]);
    let parts = [COVER, WHITE, LIFT, HIDDEN];
    assert_eq!(
        order(&h, &parts),
        [COVER, WHITE, LIFT, COVER, HIDDEN, COVER]
    );
    let runs: Vec<(Blend, Option<[f32; 4]>)> = h
        .batch
        .runs()
        .iter()
        .map(|r| (r.key.blend, r.clip_rect.map(|c| c.rect())))
        .collect();
    assert_eq!(
        runs,
        [
            (Blend::Alpha, None),
            (Blend::Add, None),
            (Blend::Alpha, Some([160.0, 820.0, 544.0, 60.0])),
            (Blend::Alpha, None),
            (Blend::Add, None),
        ]
    );
    // Before the lanes too, reading this frame.
    let before = vec![
        hidden_paint(),
        paint(0, [160, 0, 544, 100], Follow::None),
        lift_paint(),
        lanes_element(),
    ];
    let skin = Skin::new(def(Some(source()), before, paints(three_paints()))).unwrap();
    let h = shown(skin, [200, 100, 500]);
    assert_eq!(
        order(&h, &[NOTE, COVER, LIFT, HIDDEN])[..3],
        [HIDDEN, COVER, LIFT]
    );
    // LIFT's whole rect, moved up by L, is drawn under its clip.
    assert_eq!(rects(&h, LIFT), [[160.0, 820.0, 544.0, 200.0]]);
}

#[test]
fn every_paint_keeps_its_own_clock() {
    let run = |lanes: ElementDef, p: ElementDef| {
        let elements = vec![lanes, p, lift_paint(), hidden_paint()];
        let skin = Skin::new(def(Some(source()), elements, paints(three_paints()))).unwrap();
        let h = shown(skin, [200, 100, 500]);
        [COVER, HIDDEN, LIFT, NOTE].map(|part| rects(&h, part).len())
    };
    let p = || paint(0, [160, 0, 544, 100], Follow::None);
    // The lanes' timer off, or the lanes hidden: no notes, every paint on
    // its own.
    let off = ElementDef {
        timer: TimerRef::Builtin(timer::FULL_COMBO),
        ..lanes_element()
    };
    let hidden = ElementDef {
        visible: Some(bool_program("false")),
        ..lanes_element()
    };
    assert_eq!(run(off, p()), [1, 1, 1, 0]);
    assert_eq!(run(hidden, p()), [1, 1, 1, 0]);
    // A paint's timer off: the lanes and the other paints draw.
    let late = ElementDef {
        timer: TimerRef::Builtin(timer::FULL_COMBO),
        ..p()
    };
    assert_eq!(run(lanes_element(), late), [0, 1, 1, 2]);
}

#[test]
fn chosen_paint_keeps_each_entry_track_and_native_size() {
    // Entry 0 on the element's track, entry 1 [`LIFT`] on its own with a
    // native 10×20 kept in target pixels, entry 2 a hole; the selector is
    // the combo.
    let mut own = [None; 9];
    own[..4].copy_from_slice(&[Some(300), Some(40), Some(100), Some(100)]);
    let table = ImageChoiceDef {
        value: compile("combo", &Scope::new(&PLAY), Ty::Int).unwrap(),
        choices: vec![
            Some(ImageChoicePart {
                span: ImageSpan {
                    image: COVER,
                    first: 0,
                    frames: 1,
                },
                cycle: None,
                track: None,
                native: None,
            }),
            Some(ImageChoicePart {
                span: ImageSpan {
                    image: LIFT,
                    first: 0,
                    frames: 1,
                },
                cycle: Some(CycleDef {
                    period: TimeUs(100 * MS),
                    timer: TimerRef::Builtin(timer::READY),
                }),
                track: Some(lanes_support::still(own)),
                native: Some(Native::Exact([10, 20])),
            }),
            None,
        ],
    };
    let widgets = WidgetDefs {
        image_choices: vec![table],
        cover_paints: vec![CoverPaintDef {
            image: CoverImage::Choice(0),
            slider_range: Some(800),
            ..cover_image(None)
        }],
        ..WidgetDefs::default()
    };
    let elements = vec![lanes_element(), paint(0, [160, 0, 544, 100], Follow::None)];
    let skin = Skin::new(def(Some(source()), elements, widgets)).unwrap();
    let mut h = Harness::new(skin, &tap_chart(), &full(0));
    h.view.cover = 500;
    // The slider moves either entry by 400.
    h.frame(700, &[]);
    assert_eq!(rects(&h, COVER), [[160.0, 400.0, 544.0, 100.0]]);
    h.tally.combo = 1;
    for (target, scale) in [
        ([1_920, 1_080], 1.0),
        ([3_840, 2_160], 2.0),
        ([1_280, 720], 2.0 / 3.0),
    ] {
        h.target = target;
        h.frame(700, &[]);
        let [q] = quads_of(&h, LIFT)[..] else {
            panic!("one entry quad at {target:?}");
        };
        let (w, hh) = (10.0 / scale, 20.0 / scale);
        let centre = [350.0, 40.0 + 400.0 + 50.0];
        let want = [centre[0] - w / 2.0, centre[1] - hh / 2.0, w, hh];
        for (got, want) in q.rect.iter().zip(want) {
            assert!((got - want).abs() < 1e-3, "{target:?}: {:?}", q.rect);
        }
    }
    h.target = [1_920, 1_080];
    h.tally.combo = 2;
    h.frame(700, &[]);
    assert!(rects(&h, COVER).is_empty() && rects(&h, LIFT).is_empty());
}

#[test]
fn paints_are_checked_at_load() {
    let load =
        |elements: Vec<ElementDef>, widgets: WidgetDefs| Skin::new(def(None, elements, widgets));
    let one = |d: CoverPaintDef| paints(vec![d]);
    let p = |y: i32| paint(0, [0, y, 10, 10], Follow::None);
    let paint_err = |r: Result<Skin, LoadError>| match r {
        Err(LoadError::CoverPaint { paint, error }) => (paint, error),
        other => panic!("{other:?}"),
    };
    // An unreferenced definition is checked too.
    let bad_image = CoverPaintDef {
        image: CoverImage::Image(99),
        ..cover_image(None)
    };
    assert_eq!(
        paint_err(load(Vec::new(), paints(vec![cover_image(None), bad_image]))),
        (1, CoverPaintError::UnknownImage)
    );
    let bad_choice = CoverPaintDef {
        image: CoverImage::Choice(0),
        ..cover_image(None)
    };
    assert_eq!(
        paint_err(load(Vec::new(), one(bad_choice.clone()))),
        (0, CoverPaintError::UnknownChoice)
    );
    let range = |r: u32| one(cover_image(Some(r)));
    let limit = u32::try_from(MAX_COORD).unwrap();
    assert_eq!(
        paint_err(load(Vec::new(), range(limit + 1))),
        (0, CoverPaintError::Range)
    );
    // The slider's range counts in the motion check: at the bound with its
    // keys at 0, beyond with them 1 lower.
    assert!(load(vec![p(0)], range(limit)).is_ok());
    assert!(matches!(
        load(vec![p(1)], range(limit)),
        Err(LoadError::Coordinates { element: 0 })
    ));
    assert!(matches!(
        load(vec![paint(1, [0; 4], Follow::None)], one(cover_image(None))),
        Err(LoadError::UnknownWidget { element: 0 })
    ));
    let max = vec![cover_image(None); MAX_ELEMENTS];
    assert!(load(Vec::new(), paints(max.clone())).is_ok());
    let mut over = max;
    over.push(cover_image(None));
    assert!(matches!(
        load(Vec::new(), paints(over)),
        Err(LoadError::TooManyWidgets { n }) if n == MAX_ELEMENTS + 1
    ));
    // A chosen paint's entries carry their native sizes.
    let table = ImageChoiceDef {
        value: compile("0", &Scope::new(&PLAY), Ty::Int).unwrap(),
        choices: vec![None],
    };
    let chosen = WidgetDefs {
        image_choices: vec![table],
        cover_paints: vec![bad_choice],
        ..WidgetDefs::default()
    };
    let native = ElementDef {
        place: Place {
            native: Some(Native::Exact([1, 1])),
            ..Place::SCREEN
        },
        ..p(0)
    };
    assert!(matches!(
        load(vec![native], chosen),
        Err(LoadError::ChoiceNative { element: 0 })
    ));
}

#[test]
fn a_paint_is_one_quad_and_the_lanes_reserve_only_the_line() {
    let skin = skin(
        vec![
            paint(0, [0, 0, 10, 10], Follow::None),
            paint(0, [0, 0, 10, 10], Follow::Cover),
            lift_paint(),
            hidden_paint(),
        ],
        vec![
            cover_image(Some(10)),
            three_paints()[1].clone(),
            three_paints()[2].clone(),
        ],
    );
    assert_eq!(LANE_RESERVED, 1);
    assert_eq!(skin.quad_capacity(), 4 + LANE_QUADS + LANE_RESERVED);
    // Without lanes a slider still moves by the view; a follower does not.
    let skin = Skin::new(def(
        None,
        vec![
            paint(0, [0, 0, 10, 10], Follow::None),
            paint(1, [0, 0, 10, 10], Follow::Cover),
        ],
        paints(vec![cover_image(Some(100)), cover_image(None)]),
    ))
    .unwrap();
    assert_eq!(skin.quad_capacity(), 2);
    let mut h = Harness::new(skin, &tap_chart(), &full(0));
    h.view = LaneView {
        cover: 500,
        ..h.view
    };
    h.frame(700, &[]);
    assert_eq!(ys(&h), [50.0, 0.0]);
}

#[test]
fn the_lanes_leave_the_variant_to_the_lane_width() {
    // At 3840×2160 (scale 2) with the lanes at 50 %: the lanes are 1, not
    // magnified.
    let only_lanes = |lanes: ElementDef| {
        Skin::new(def(Some(simple()), vec![lanes], WidgetDefs::default())).unwrap()
    };
    let mut half = Layout::AUTHOR;
    half.set_lane_width(50).unwrap();
    let skin = only_lanes(lanes_element());
    assert_eq!(skin.variant([3_840, 2_160], &half), Variant::Lo);
    assert_eq!(skin.variant([3_840, 2_160], &Layout::AUTHOR), Variant::Hi);
    // Lanes never drawn do not count.
    let never = ElementDef {
        visible: Some(bool_program("false")),
        ..lanes_element()
    };
    let skin = only_lanes(never);
    assert_eq!(skin.variant([3_840, 2_160], &Layout::AUTHOR), Variant::Lo);
}
