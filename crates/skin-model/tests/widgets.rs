//! What widgets cost and how they pick assets: `variant` counts widget
//! elements like images of their region, `quad_capacity` holds each
//! widget's worst case, and the skin-wide widget quad cap.

mod lanes_support;
mod play_support;
mod widget_support;

use bms_model::TimeUs;
use rules::Judgment;
use skin_model::{
    Adjust, Align, Bind, DigitsDef, ElementDef, Fit, GlyphSet, JudgeSlot, Layout, LoadError,
    MAX_SEGMENTS, MAX_WIDGET_QUADS, Motion, Pad, Place, Region, Source, Variant, WidgetDefs,
};
use widget_support::{
    TEN, combo, digits, element, gauge, harness, judge_all, judge_element, judgment, load,
    load_fit, number,
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
