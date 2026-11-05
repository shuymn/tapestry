//! What a frame's draw evaluated of each element and the display list a
//! capture reads from it (dev-tools): each way an element stops, the values
//! it reached, its quads' attribution and sources, and the quads an owner
//! pushes after the skin's elements.

#![cfg(feature = "dev-tools")]

mod lanes_support;
mod play_support;
mod widget_support;

use bms_model::TimeUs;
use lanes_support::{HIDDEN, LIFT, lanes_element, moving, paint, skin_with_widgets, source};
use play_support::{Harness, REGIONS, full, readouts_for};
use skin_model::{
    Adjust, Align, Blend, ChartText, CoverMotion, CoverPaintDef, CoverTrim, Ease, ElementDef, End,
    Follow, GlyphSet, ImageChoiceDef, ImageChoicePart, ImageSpan, KeyDef, Outcome, Pad, PastTable,
    Place, Quad, QuadBatch, QuadSource, Region, RunKey, Skin, Source, TimerRef, Track, WidgetDefs,
    timer,
};
use widget_support::{TEN, TEN_CYCLED, boolean, digits, element, harness, int, load, number};

const KEY: [i32; 4] = [100, 100, 40, 40];

/// The observation of element `element` after `h`'s last frame.
fn observed(h: &Harness, element: u32) -> skin_model::ElementObservation {
    *h.batch
        .observations()
        .iter()
        .find(|o| o.element == element)
        .unwrap()
}

/// An image choice of `choices` selecting by `value`.
fn choice(value: &str, choices: Vec<Option<ImageChoicePart>>) -> ImageChoiceDef {
    ImageChoiceDef {
        value: int(value),
        past_table: PastTable::Hide,
        choices,
    }
}

/// An entry of `image`'s first frame.
fn entry(image: u32) -> ImageChoicePart {
    ImageChoicePart {
        span: ImageSpan {
            image,
            first: 0,
            frames: 1,
        },
        cycle: None,
        track: None,
        native: None,
    }
}

/// An image element of [`widget_support`]'s sheet.
fn image(i: u32) -> ElementDef {
    element(Source::Image(i), KEY, Place::SCREEN)
}

/// A skin of each outcome the plain widgets reach, in order: drawn, its
/// visibility false, its timer off, a number, a selected choice, a hole in
/// a choice, a cycle whose timer is off, and a hidden region's.
fn outcomes() -> Skin {
    let hidden = ElementDef {
        visible: Some(boolean("false")),
        ..image(TEN)
    };
    let timer_off = ElementDef {
        timer: TimerRef::Builtin(timer::FAILED),
        ..image(TEN)
    };
    let in_region = element(
        Source::Image(TEN),
        KEY,
        Place {
            region: Region::Judge,
            ..Place::SCREEN
        },
    );
    load(
        vec![
            image(TEN),
            hidden,
            timer_off,
            element(Source::Number(0), KEY, Place::SCREEN),
            element(Source::ImageChoice(0), KEY, Place::SCREEN),
            element(Source::ImageChoice(1), KEY, Place::SCREEN),
            image(TEN_CYCLED),
            in_region,
        ],
        WidgetDefs {
            numbers: vec![number(
                digits(TEN, GlyphSet::Ten, 4, Align::Right, Pad::Blank),
                "42",
            )],
            image_choices: vec![
                choice("1", vec![Some(entry(TEN)), Some(entry(TEN))]),
                choice("0", vec![None]),
            ],
            ..WidgetDefs::default()
        },
    )
    .unwrap()
}

/// Each way an element stops is said at its element, with what it had
/// evaluated by then: the sampled time, a number's value, a chosen entry.
#[test]
fn an_observation_says_where_each_element_stopped() {
    let mut h = harness(outcomes());
    let mut layout = skin_model::Layout::AUTHOR;
    layout
        .set(
            Region::Judge,
            Adjust {
                visible: false,
                ..Adjust::IDENTITY
            },
        )
        .unwrap();
    h.layout = layout;
    h.frame(250, &[]);
    assert_eq!(h.batch.observations().len(), 8);
    assert_eq!(observed(&h, 0).outcome, Outcome::Sampled);
    // On `ready`, from the play's lead-in.
    assert!(observed(&h, 0).t.is_some_and(|t| t > TimeUs(0)));
    assert_eq!(observed(&h, 1).outcome, Outcome::NotSampled);
    assert_eq!(observed(&h, 1).t, None);
    assert_eq!(observed(&h, 2).outcome, Outcome::NotSampled);
    let number = observed(&h, 3);
    assert_eq!((number.outcome, number.value), (Outcome::Sampled, Some(42)));
    let selected = observed(&h, 4);
    assert_eq!(
        (selected.outcome, selected.choice),
        (Outcome::Sampled, Some(1))
    );
    assert_eq!(observed(&h, 5).outcome, Outcome::NoChoice);
    assert_eq!(observed(&h, 6).outcome, Outcome::Sampled);
    assert_eq!(observed(&h, 7).outcome, Outcome::RegionHidden);
}

/// A cover paint's own stops: HIDDEN+'s while it is off, a gate without a
/// sample, and a trim line above the paint.
#[test]
fn an_observation_says_where_a_cover_paint_stopped() {
    let late = Track::new(
        &[KeyDef {
            at: TimeUs(9_000_000),
            props: [
                Some(0),
                Some(0),
                Some(1),
                Some(1),
                None,
                None,
                None,
                None,
                None,
            ],
        }],
        Ease::Linear,
        End::Hold,
    )
    .unwrap();
    let defs = vec![
        moving(HIDDEN, CoverMotion::Hidden, CoverTrim::None),
        CoverPaintDef {
            gate_track: Some(late),
            ..moving(LIFT, CoverMotion::Lift, CoverTrim::Fixed(105))
        },
        moving(LIFT, CoverMotion::Lift, CoverTrim::Fixed(10)),
    ];
    let skin = skin_with_widgets(
        REGIONS,
        Some(source()),
        vec![
            lanes_element(),
            paint(0, [160, 280, 544, 600], Follow::None),
            paint(1, [160, 55, 544, 100], Follow::None),
            paint(2, [160, 55, 544, 100], Follow::None),
        ],
        WidgetDefs {
            cover_paints: defs,
            ..WidgetDefs::default()
        },
    )
    .unwrap();
    let mut h = Harness::new(skin, &lanes_support::tap_chart(), &full(0));
    let mut readouts = readouts_for(h.view);
    readouts.covers.hidden_enabled = false;
    h.readouts = Some(readouts);
    h.frame(0, &[]);
    assert_eq!(observed(&h, 1).outcome, Outcome::HiddenOff);
    assert_eq!(observed(&h, 2).outcome, Outcome::GateNotSampled);
    assert_eq!(observed(&h, 3).outcome, Outcome::TrimSuppressed);
}

/// A list attributes each quad to its element by the observations' spans,
/// says the file and rectangle it shows, and leaves what an owner pushes
/// after the skin's elements to no element; a text element's string is its
/// chart label's.
#[test]
fn a_display_list_attributes_quads_and_their_sources() {
    let mut h = harness(outcomes());
    h.frame(250, &[]);
    let skin_quads = h.batch.quads().len();
    h.batch.open_reserve();
    let key = RunKey {
        blend: Blend::Add,
        clip: None,
    };
    assert!(h.batch.push(key, Quad::default()));
    let chart = ChartText::default();
    let list = h.skin.display_list(&h.batch, &h.uvs, &chart);
    assert_eq!(list.element_count, 8);
    assert_eq!(list.observations_dropped, 0);
    assert_eq!(list.quads.len(), skin_quads + 1);
    // The first image element's first quad, then the number's digits.
    let first = list.quads.first().unwrap();
    assert_eq!(first.element, Some(0));
    assert_eq!(first.blend, Blend::Alpha);
    let QuadSource::File { name, rect, .. } = &first.source else {
        panic!("{first:?}");
    };
    assert_eq!(name, &format!("{TEN}.png"));
    assert_eq!(rect[2..], [2, 3]);
    assert!(list.quads.iter().any(|q| q.element == Some(3)));
    // The quad pushed after the elements is nobody's.
    let last = list.quads.last().unwrap();
    assert_eq!(last.element, None);
    assert_eq!(last.blend, Blend::Add);
}

/// A batch of too few places counts what it could not observe, and what it
/// could stays right; a new frame starts over.
#[test]
fn clearing_a_batch_starts_its_observations_over() {
    let mut h = harness(outcomes());
    h.frame(250, &[]);
    assert_eq!(h.batch.observations().len(), 8);
    assert_eq!(
        h.batch.observation_end().map(|e| e as usize),
        Some(h.batch.quads().len())
    );
    h.batch.clear();
    assert!(h.batch.observations().is_empty());
    assert_eq!(h.batch.observation_end(), None);
    let fresh = QuadBatch::with_capacity(4, 4);
    assert_eq!(fresh.observations_dropped(), 0);
}
