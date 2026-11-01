//! What widgets cost and how they pick assets: `variant` counts widget
//! elements like images of their region, `quad_capacity` holds each
//! widget's worst case, and the skin-wide widget quad cap.

mod lanes_support;
mod play_support;
mod widget_support;

use bms_model::TimeUs;
use rules::Judgment;
use skin_model::{
    Adjust, Align, Bind, ComboPart, DigitsDef, ElementDef, Fit, Follow, GlyphSet, GradePart,
    JudgeSlot, Layout, LoadError, MAX_SEGMENTS, MAX_WIDGET_QUADS, Motion, Pad, Place, Region,
    Source, TimerRef, Variant, WidgetDefs, timer,
};
use widget_support::{
    ELEVEN, GAUGE, GRADE, TEN, combo, digits, drawn, element, gauge, grade, harness, judge_all,
    judge_element, judgment, load, load_fit, number, shown,
};

fn adjust(scale: u16) -> Adjust {
    Adjust {
        x: 0,
        y: 0,
        scale,
        visible: true,
    }
}

#[test]
fn widgets_pick_the_variant_like_images_of_their_region() {
    // A skin whose only images are a judge's.
    let skin = load(
        vec![judge_element(0)],
        WidgetDefs {
            judges: vec![judge_all([800, 500, 200, 80])],
            ..WidgetDefs::default()
        },
    )
    .unwrap();
    let mut layout = Layout::AUTHOR;
    assert_eq!(skin.variant([1_920, 1_080], &layout), Variant::Lo);
    layout.set(Region::Judge, adjust(120)).unwrap();
    assert_eq!(skin.variant([1_920, 1_080], &layout), Variant::Hi);
    // A bound gauge at 50 % lane width on 3840×2160: 2 · 0.5 is not a
    // magnification; unbound it is.
    let gauge_skin = |bind: Bind| {
        load_fit(
            Fit::Adapt,
            play_support::REGIONS,
            Some(lanes_support::simple()),
            vec![
                lanes_support::lanes_element(),
                element(
                    Source::Gauge(0),
                    [200, 50, 300, 20],
                    Place {
                        region: Region::Playfield,
                        bind,
                        ..Place::SCREEN
                    },
                ),
            ],
            WidgetDefs {
                gauges: vec![gauge(50, Motion::Still)],
                ..WidgetDefs::default()
            },
        )
        .unwrap()
    };
    let mut narrow = Layout::AUTHOR;
    narrow.set_lane_width(50).unwrap();
    assert_eq!(
        gauge_skin(Bind::Lanes).variant([3_840, 2_160], &narrow),
        Variant::Lo
    );
    assert_eq!(
        gauge_skin(Bind::None).variant([3_840, 2_160], &narrow),
        Variant::Hi
    );
}

/// An image, an 11-cell number, a 50-segment flicker gauge and a judge
/// with every grade and 4-cell combos on three slots.
fn worst() -> skin_model::Skin {
    let mut j = judge_all([800, 500, 200, 80]);
    let strip = DigitsDef {
        space: 0,
        ..digits(TEN, GlyphSet::Ten, 4, Align::Right, Pad::Zero)
    };
    for s in [JudgeSlot::Perfect, JudgeSlot::Great, JudgeSlot::Good] {
        j.combos[s.index()] = Some(combo(strip, [210, 10, 20, 60]));
    }
    load(
        vec![
            element(Source::Image(0), [0, 0, 10, 10], Place::SCREEN),
            element(Source::Number(0), [100, 50, 20, 30], Place::SCREEN),
            element(Source::Gauge(0), [0, 100, 500, 20], Place::SCREEN),
            judge_element(0),
        ],
        WidgetDefs {
            numbers: vec![number(
                digits(TEN, GlyphSet::Ten, 11, Align::Right, Pad::Zero),
                "combo",
            )],
            gauges: vec![gauge(
                50,
                Motion::Flicker {
                    period: TimeUs(1_000_000),
                },
            )],
            judges: vec![j],
            texts: Vec::new(),
            image_choices: Vec::new(),
        },
    )
    .unwrap()
}

#[test]
fn capacity_holds_each_widget_s_worst_case() {
    // 1 + 11 + 51 + (1 + 4): a judge counts its largest slot, not the
    // sum of its slots (7 + 3·4).
    let skin = worst();
    assert_eq!(skin.quad_capacity(), 68);
    assert_eq!(skin.run_capacity(), 68);
    let mut h = harness(skin);
    h.tally = play_support::tally(1_234);
    h.gauges[0].value = play_support::pct(50);
    h.frame(500, &[judgment(500, Judgment::Great, 1_234)]);
    assert_eq!(h.batch.quads().len(), 68);
    assert!(h.batch.is_full());
    assert_eq!(h.batch.dropped(), 0);
}

#[test]
fn widget_quads_are_capped_per_skin() {
    // 16 flicker gauges of 1 000 segments push 16 016; numbers of 368
    // cells reach the cap exactly.
    let numbers = |last: u8| {
        let mut cells = vec![11; 33];
        cells.push(last);
        cells
            .into_iter()
            .map(|n| number(digits(TEN, GlyphSet::Ten, n, Align::Right, Pad::Blank), "1"))
            .collect::<Vec<_>>()
    };
    let with = |last: u8| {
        let mut elements: Vec<ElementDef> = (0..16)
            .map(|_| element(Source::Gauge(0), [0, 0, 1_000, 10], Place::SCREEN))
            .collect();
        elements.extend((0..34).map(|i| element(Source::Number(i), [0, 0, 10, 10], Place::SCREEN)));
        load(
            elements,
            WidgetDefs {
                numbers: numbers(last),
                gauges: vec![gauge(
                    MAX_SEGMENTS,
                    Motion::Flicker {
                        period: TimeUs(1_000),
                    },
                )],
                ..WidgetDefs::default()
            },
        )
    };
    assert_eq!(16 * 1_001 + 33 * 11 + 5, MAX_WIDGET_QUADS);
    let skin = with(5).unwrap();
    assert_eq!(skin.quad_capacity(), MAX_WIDGET_QUADS);
    assert!(matches!(
        with(6),
        Err(LoadError::TooManyWidgetQuads { n: 16_385 })
    ));
}

#[test]
fn followed_widgets_move_once_by_the_covers() {
    // The simple lanes are 800 long. A lift of 250 ‰ raises the line
    // L = 200 and leaves 600; a cover of 400 ‰ takes C = 240 of it. The
    // number follows the cover (+240), the gauge both (+40), GREAT's grade
    // the lift (−200); its combo, on another timer, attaches to the moved
    // grade and moves with it once, the shift unchanged. The image follows
    // nothing. The lanes' own timer is off.
    let lanes = ElementDef {
        timer: TimerRef::Builtin(timer::FULL_COMBO),
        ..lanes_support::lanes_element()
    };
    let at = |follow| Place {
        follow,
        ..Place::SCREEN
    };
    let mut j = judge_all([800, 500, 200, 80]);
    j.grades[JudgeSlot::Great.index()] = Some(GradePart {
        follow: Follow::Lift,
        ..grade(GRADE + 1, [800, 500, 200, 80])
    });
    j.combos[JudgeSlot::Great.index()] = Some(ComboPart {
        timer: TimerRef::Builtin(timer::READY),
        ..combo(
            digits(TEN, GlyphSet::Ten, 4, Align::Center, Pad::Blank),
            [210, 10, 20, 60],
        )
    });
    let skin = load_fit(
        Fit::Adapt,
        play_support::REGIONS,
        Some(lanes_support::simple()),
        vec![
            lanes,
            element(Source::Number(0), [100, 50, 20, 30], at(Follow::Cover)),
            element(
                Source::Gauge(0),
                [0, 100, 500, 20],
                at(Follow::LiftAndCover),
            ),
            judge_element(0),
            element(Source::Image(0), [0, 0, 10, 10], at(Follow::None)),
        ],
        WidgetDefs {
            numbers: vec![number(
                digits(ELEVEN, GlyphSet::Eleven, 3, Align::Right, Pad::Blank),
                "123",
            )],
            gauges: vec![gauge(10, Motion::Still)],
            judges: vec![j],
            texts: Vec::new(),
            image_choices: Vec::new(),
        },
    )
    .unwrap();
    let mut h = harness(skin);
    h.tally = play_support::tally(12);
    h.frame(500, &[judgment(500, Judgment::Great, 12)]);
    let before = drawn(&h);
    h.view.lift = 250;
    h.view.cover = 400;
    h.frame(500, &[]);
    let after = drawn(&h);
    assert_eq!(before.len(), after.len());
    let mut seen = [false; 5];
    for ((image, frame, a), (i2, f2, b)) in before.into_iter().zip(after) {
        assert_eq!((image, frame), (i2, f2));
        let (k, dy) = match image {
            ELEVEN => (0, 240.0),
            GAUGE.. => (1, 40.0),
            TEN => (2, -200.0),
            g if g == GRADE + 1 => (3, -200.0),
            _ => (4, 0.0),
        };
        seen[k] = true;
        let [x, y, w, hh] = a.rect;
        assert_eq!(b.rect, [x, y + dy, w, hh], "image {image}");
    }
    assert_eq!(seen, [true; 5]);
}

#[test]
fn followers_keep_the_transform_order() {
    // A lift follower moves before its lane binding and its region: at a
    // region scale of 200 % it moves twice as far on the canvas, its x
    // and width follow the scratch side and the lane width as unmoved,
    // its pivot moves with it, and its clip and run stay.
    let follower = |follow| {
        let place = Place {
            follow,
            pivot: skin_model::Anchor::Center,
            ..lanes_support::bound(Bind::Lane(bms_model::Lane::SCRATCH))
        };
        let mut props = [None; 9];
        props[..4].copy_from_slice(&[Some(160), Some(400), Some(96), Some(20)]);
        props[8] = Some(30);
        ElementDef {
            track: lanes_support::still(props),
            clip: Some(skin_model::ClipId(2)),
            ..element(Source::Image(0), [0; 4], place)
        }
    };
    let quads = |follow, lift: u16, scratch_right: bool| {
        let skin = load_fit(
            Fit::Adapt,
            play_support::REGIONS,
            Some(lanes_support::simple()),
            vec![lanes_support::lanes_element(), follower(follow)],
            WidgetDefs::default(),
        )
        .unwrap();
        let ctx = skin_model::PlayContext {
            scratch: if scratch_right {
                skin_model::Side::Right
            } else {
                skin_model::Side::Left
            },
            ..play_support::full(0)
        };
        let mut h = play_support::Harness::new(skin, &widget_support::chart(), &ctx);
        let mut layout = Layout::AUTHOR;
        layout
            .set(
                Region::Playfield,
                Adjust {
                    x: 30,
                    y: -20,
                    scale: 200,
                    visible: true,
                },
            )
            .unwrap();
        layout.set_lane_width(150).unwrap();
        h.layout = layout;
        h.target = [2_560, 1_080];
        h.view.lift = lift;
        h.frame(0, &[]);
        let q = h
            .batch
            .quads()
            .iter()
            .find(|q| shown(&h, q) == Some((0, 0)))
            .copied()
            .unwrap();
        let run = h
            .batch
            .runs()
            .iter()
            .find(|r| (r.start..r.end).any(|i| h.batch.quads()[i as usize].uv == q.uv))
            .map(|r| r.key)
            .unwrap();
        (q, run)
    };
    for right in [false, true] {
        let (still, still_run) = quads(Follow::None, 250, right);
        let (moved, moved_run) = quads(Follow::Lift, 250, right);
        let (unlifted, _) = quads(Follow::Lift, 0, right);
        // L = 200 reference pixels, 400 on the canvas at 200 %.
        let [x, y, w, hh] = still.rect;
        assert_eq!(moved.rect, [x, y - 400.0, w, hh], "right {right}");
        assert_eq!(moved.pivot, [still.pivot[0], still.pivot[1] - 400.0]);
        assert_eq!(moved.angle, still.angle);
        assert_eq!(unlifted.rect, still.rect);
        assert_eq!(moved_run, still_run);
        assert_eq!(moved_run.clip, Some(skin_model::ClipId(2)));
    }
}

#[test]
fn judges_follow_only_through_their_grades() {
    let following = ElementDef {
        place: Place {
            region: Region::Judge,
            follow: Follow::Cover,
            ..Place::SCREEN
        },
        ..judge_element(0)
    };
    let widgets = || WidgetDefs {
        judges: vec![judge_all([800, 500, 200, 80])],
        ..WidgetDefs::default()
    };
    assert!(matches!(
        load(vec![following], widgets()),
        Err(LoadError::UnsupportedFollow { element: 0 })
    ));
    // A followed grade's keys moved the lanes' 800 stay within ±MAX_COORD.
    let mut j = judge_all([800, 500, 200, 80]);
    j.grades[2] = Some(GradePart {
        follow: Follow::Cover,
        ..grade(GRADE + 2, [800, skin_model::MAX_COORD - 799, 200, 80])
    });
    let r = load_fit(
        Fit::Adapt,
        play_support::REGIONS,
        Some(lanes_support::simple()),
        vec![lanes_support::lanes_element(), judge_element(0)],
        WidgetDefs {
            judges: vec![j],
            ..WidgetDefs::default()
        },
    );
    assert!(matches!(
        r,
        Err(LoadError::Judge {
            element: 1,
            error: skin_model::JudgeError::Coordinates {
                part: skin_model::JudgePart::Grade(JudgeSlot::Good)
            }
        })
    ));
}
