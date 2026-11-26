//! Skin movies in a skin (design.md "Rendering and Skins", skin movies):
//! a movie element draws what the frame's table shows for its slot (its
//! frame, sampled from the movies' planes by its matrix and range and met
//! by its stretch against the movie's size; nothing; or opaque black over
//! the destination whatever its blend and colour once lost), in play and
//! loading frames alike, and only the play and selection skins may draw
//! one.

mod lanes_support;
mod play_support;
mod widget_support;

use skin_model::bga::{BgaStretch, SOLID};
use skin_model::chart_image::ChartStretch;
use skin_model::media::ChromaLocation;
use skin_model::movie::{MovieShow, MovieShows};
use skin_model::{
    Blend, ElementDef, Filter, LoadError, MovieSampling, Place, Quad, ScreenKind, Skin, Source,
    TimerRef, timer,
};

use lanes_support::rect;
use play_support::{Harness, VIEW, def, full, readouts_for, uvs, white_sheet};
use widget_support::chart;

const DST: [i32; 4] = [100, 100, 512, 512];
const FILL: BgaStretch = BgaStretch::Whole(ChartStretch::Fill);

fn movie_element(slot: u8, stretch: BgaStretch, blend: Blend) -> ElementDef {
    ElementDef {
        source: Source::Movie { slot, stretch },
        track: rect(DST),
        timer: TimerRef::Builtin(timer::SCREEN),
        visible: None,
        blend,
        clip: None,
        place: Place::SCREEN,
    }
}

fn sheet() -> skin_model::Sheet {
    white_sheet().with_movies(["mv/a.mp4".to_owned(), "mv/b.mp4".to_owned()])
}

fn harness(elements: Vec<ElementDef>) -> Harness {
    let skin = Skin::new(def(Vec::new(), sheet(), elements)).unwrap();
    Harness::new(skin, &chart(), &full(1_000))
}

const FRAME: MovieShow = MovieShow::Frame {
    uv: [0.0, 0.0, 0.5, 0.25],
    layer: 1,
    size: [1920, 1080],
    bt601: false,
    full: true,
    chroma: ChromaLocation::Left,
};

/// The filter [`FRAME`] draws with.
const FRAME_FILTER: Filter = Filter::Movie {
    sampling: MovieSampling::new(false, true, ChromaLocation::Left),
};

fn shows(first: MovieShow, second: MovieShow) -> MovieShows {
    [first, second, MovieShow::Nothing, MovieShow::Nothing]
}

/// The frame's quads after a build with `movies`.
fn quads(h: &mut Harness, movies: MovieShows) -> Vec<Quad> {
    h.play.set_movies(movies);
    h.frame(0, &[]);
    h.batch.quads().to_vec()
}

#[test]
fn a_live_movie_draws_its_frame_from_its_layer_by_its_colour() {
    let mut h = harness(vec![movie_element(0, FILL, Blend::Add)]);
    let q = quads(&mut h, shows(FRAME, MovieShow::Nothing));
    assert_eq!(q.len(), 1);
    let q = q[0];
    assert_eq!(q.filter, FRAME_FILTER);
    assert_eq!((q.uv, q.layer), ([0.0, 0.0, 0.5, 0.25], 1));
    assert_eq!(
        q.rect,
        [100.0, 100.0, 512.0, 512.0],
        "filling its destination"
    );
    assert_eq!(
        h.batch.runs()[0].key.blend,
        Blend::Add,
        "the destination's blend"
    );
}

#[test]
fn a_movie_keeps_its_stretch_against_its_own_size() {
    let inner = BgaStretch::Whole(ChartStretch::Inner);
    let mut h = harness(vec![movie_element(0, inner, Blend::Alpha)]);
    let q = quads(&mut h, shows(FRAME, MovieShow::Nothing));
    // 1920×1080 inside 512×512: 512×288, centred.
    assert_eq!(q[0].rect, [100.0, 212.0, 512.0, 288.0]);
}

#[test]
fn a_movie_with_nothing_to_show_draws_nothing() {
    let mut h = harness(vec![movie_element(0, FILL, Blend::Alpha)]);
    assert!(quads(&mut h, shows(MovieShow::Nothing, FRAME)).is_empty());
    // The other slot's frame is not this element's.
    let mut h = harness(vec![movie_element(1, FILL, Blend::Alpha)]);
    assert_eq!(quads(&mut h, shows(FRAME, MovieShow::Nothing)).len(), 0);
}

/// The batch marks the movie slots whose frame it holds a quad of
/// (design.md "Rendering and Skins", skin movies: a movie's clock starts at
/// the first presented frame that draws it), and nothing for a slot with
/// nothing to show, a lost one's black or a skin with no element of it;
/// each frame's build starts the marks afresh.
// Revert: the movie quad pushed without its mark -> "slot 1's frame
// drawn".
#[test]
fn a_batch_marks_the_movie_slots_whose_frame_it_drew() {
    let mut h = harness(vec![movie_element(1, FILL, Blend::Alpha)]);
    let _ = quads(&mut h, shows(FRAME, FRAME));
    assert_eq!(
        h.batch.movies(),
        [false, true, false, false],
        "slot 1's frame drawn"
    );
    let _ = quads(&mut h, shows(FRAME, MovieShow::Lost));
    assert_eq!(h.batch.movies(), [false; 4], "a lost movie's black");
    let _ = quads(&mut h, shows(FRAME, MovieShow::Nothing));
    assert_eq!(h.batch.movies(), [false; 4], "nothing to show");
}

#[test]
fn a_lost_movie_draws_opaque_black_whatever_its_blend_and_colour() {
    for blend in [Blend::Alpha, Blend::Add, Blend::Multiply] {
        let mut h = harness(vec![movie_element(0, FILL, blend)]);
        let q = quads(&mut h, shows(MovieShow::Lost, MovieShow::Nothing));
        assert_eq!(q.len(), 1, "{blend:?}");
        assert_eq!((q[0].uv, q[0].layer), (SOLID.uv, SOLID.layer));
        assert_eq!(q[0].color, [0.0, 0.0, 0.0, 1.0], "{blend:?}");
        assert_eq!(q[0].rect, [100.0, 100.0, 512.0, 512.0]);
        assert_eq!(h.batch.runs()[0].key.blend, Blend::Alpha, "{blend:?}");
    }
}

#[test]
fn a_movie_keeps_its_place_in_the_draw_order() {
    let image = ElementDef {
        source: Source::Image(0),
        ..movie_element(0, FILL, Blend::Alpha)
    };
    let mut h = harness(vec![
        image.clone(),
        movie_element(0, FILL, Blend::Alpha),
        image,
    ]);
    let q = quads(&mut h, shows(FRAME, MovieShow::Nothing));
    let filters: Vec<Filter> = q.iter().map(|q| q.filter).collect();
    assert_eq!(filters, [Filter::Linear, FRAME_FILTER, Filter::Linear]);
}

#[test]
fn only_play_and_selection_skins_draw_a_movie_of_a_slot_they_have() {
    use skin_model::{DecisionDefs, ResultDefs, SelectionDefs};
    let on = |t| {
        vec![ElementDef {
            timer: TimerRef::Builtin(t),
            ..movie_element(0, FILL, Blend::Alpha)
        }]
    };
    let selection = Skin::new_selection(
        def(Vec::new(), sheet(), on(skin_model::selection_timer::SCREEN)),
        SelectionDefs::default(),
    );
    assert!(selection.is_ok(), "{:?}", selection.err());
    let result = Skin::new_result(
        def(Vec::new(), sheet(), on(skin_model::result_timer::SCREEN)),
        ResultDefs::default(),
    );
    let decision = Skin::new_decision(
        def(
            Vec::new(),
            sheet(),
            vec![movie_element(0, FILL, Blend::Alpha)],
        ),
        DecisionDefs::default(),
    );
    for (kind, refused) in [
        (ScreenKind::Result, result.err()),
        (ScreenKind::Decision, decision.err()),
    ] {
        assert!(
            matches!(refused, Some(LoadError::SourceKind { .. })),
            "{kind:?}: {refused:?}"
        );
    }
    // A slot the sheet has not.
    let none = Skin::new(def(
        Vec::new(),
        white_sheet(),
        vec![movie_element(0, FILL, Blend::Alpha)],
    ));
    let none = none.err();
    assert!(
        matches!(none, Some(LoadError::UnknownWidget { element: 0 })),
        "{none:?}"
    );
}

/// The loading frame's quads of a play skin with `elements`, its movies
/// showing `movies`.
fn loading_quads(elements: Vec<ElementDef>, movies: MovieShows) -> Vec<Quad> {
    use skin_model::{Layout, LoadState, LoadingFrame, QuadBatch};
    let skin = Skin::new(def(Vec::new(), sheet(), elements)).unwrap();
    let mut loading = skin.prepare_loading(skin_model::Side::Left);
    loading.set_movies(movies);
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let layout = Layout::AUTHOR;
    let frame = LoadingFrame {
        app: skin_model::AppReadouts::LAUNCH,
        display_offset: bms_model::TimeUs(0),
        fix_hispeed: 0,
        difficulty: None,
        bga: None,
        length: None,
        autoplay: false,
        now: bms_model::TimeUs(16_000),
        load: LoadState {
            since: bms_model::TimeUs(0),
            percent: 0,
        },
        target: [1_920, 1_080],
        layout: &layout,
        turntable: 0,
        lanes: VIEW,
        covers: readouts_for(VIEW).covers,
        gauge: None,
        play_level: None,
    };
    skin.build_loading(&mut loading, &frame, &uvs(&skin), &mut batch)
        .unwrap();
    batch.quads().to_vec()
}

/// A loading frame draws the movies as a play frame does: a ready
/// movie's frame, and opaque black for a lost one; nothing for one with
/// nothing to show.
#[test]
fn a_loading_frame_draws_a_ready_movie_and_a_lost_one_black() {
    let elements = vec![
        movie_element(0, FILL, Blend::Add),
        movie_element(1, FILL, Blend::Add),
    ];
    let q = loading_quads(elements.clone(), shows(FRAME, MovieShow::Lost));
    assert_eq!(q.len(), 2, "{q:?}");
    assert_eq!(q[0].filter, FRAME_FILTER);
    assert_eq!((q[0].uv, q[0].layer), ([0.0, 0.0, 0.5, 0.25], 1));
    assert_eq!((q[1].uv, q[1].layer), (SOLID.uv, SOLID.layer));
    assert_eq!(q[1].color, [0.0, 0.0, 0.0, 1.0]);
    let none = loading_quads(elements, shows(MovieShow::Nothing, MovieShow::Nothing));
    assert!(none.is_empty(), "{none:?}");
}
