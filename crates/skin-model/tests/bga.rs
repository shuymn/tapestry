//! The chart's BGA in a play skin (design.md "Rendering and Skins", BGA):
//! a `bga` element draws the base and the layer in effect at the frame's
//! chart time, never the display time; the poor pictures replace both for
//! 500 ms after a judgment that leaves the combo at 0, once a poor
//! sequence has passed; the black placeholder before chart time 0, for a
//! base with no picture and while the table is not drawable; a cropping
//! stretch crops at each frame against the picture's drawn size; and only
//! a play skin may draw it. A video id draws its chart movie slot's frame,
//! placed against the video's size, on every channel, or its channel's
//! absence while the slot shows nothing or no slots are set; a lost chart
//! video draws one opaque black quad over each element.

mod lanes_support;
mod play_support;
mod widget_support;

use std::sync::Arc;

use bms_model::{BgaEvent, BgaImage, BgaImageId, ChartBga, PoorSequence, TimeUs};
use rules::Judgment;
use skin_model::bga::{BgaCell, BgaEntry, BgaKey, BgaStretch, BgaTable, ChartVideos, Crop, SOLID};
use skin_model::chart_image::ChartStretch;
use skin_model::movie::{MovieShow, MovieShows};
use skin_model::{
    ElementDef, Filter, FrameUv, Place, PlayContext, Quad, ScreenKind, Skin, Source, TimerRef,
    timer,
};

use lanes_support::rect;
use play_support::{Harness, MS, empty_poor, ev, full, judged, load, white_sheet};
use widget_support::chart;

const KEY: BgaKey = BgaKey {
    renderer: 1,
    array: 7,
};
const DST: [i32; 4] = [100, 100, 512, 512];

/// Picture `i`'s cell: its own uv on page `i`, and the drawn size.
fn cell(i: u16, native: [u32; 2]) -> BgaEntry {
    let u = f32::from(i) / 16.0;
    BgaEntry::Picture(BgaCell {
        uv: FrameUv {
            uv: [u, 0.0, u + 0.0625, 0.0625],
            layer: i,
        },
        native,
    })
}

fn image(id: u16) -> BgaImage {
    BgaImage {
        id,
        name: format!("{id}.png"),
    }
}

fn event(ms: i64, image: Option<u16>) -> BgaEvent {
    BgaEvent {
        time: TimeUs(ms * MS),
        image: image.map(BgaImageId),
    }
}

/// Base 0 from 0 ms, base 1 from 1 000 ms, the base cleared at 2 000 ms;
/// layer 2 from 500 ms; a poor sequence of 3 then 4 at 3 000 ms.
fn bga() -> ChartBga {
    ChartBga::new(
        (1..=5).map(image).collect(),
        vec![event(0, Some(0)), event(1_000, Some(1)), event(2_000, None)],
        vec![event(500, Some(2))],
        vec![PoorSequence {
            time: TimeUs(3_000 * MS),
            frames: vec![Some(BgaImageId(3)), Some(BgaImageId(4))].into_boxed_slice(),
        }],
    )
    .unwrap()
}

const FILL: BgaStretch = BgaStretch::Whole(ChartStretch::Fill);

fn bga_element(stretch: BgaStretch) -> ElementDef {
    ElementDef {
        source: Source::Bga { stretch },
        track: rect(DST),
        timer: TimerRef::Builtin(timer::SCREEN),
        visible: None,
        blend: skin_model::Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    }
}

fn skin(stretch: BgaStretch) -> Skin {
    load(Vec::new(), vec![bga_element(stretch)]).unwrap()
}

/// Every picture present, filling (stretch 0).
fn table() -> BgaTable {
    BgaTable::new((0..5).map(|i| cell(i, [256, 256])).collect()).keyed(KEY)
}

fn harness(stretch: BgaStretch, table: BgaTable) -> Harness {
    let mut c = chart();
    c.bga = Some(bga());
    let mut h = Harness::new(skin(stretch), &c, &full(1_000));
    h.play.set_bga_images(Arc::new(table));
    h.play.set_bga_live(Some(KEY));
    h
}

/// What each quad drew: its source (`Some(page)` for a BGA picture,
/// `None` for the placeholder) and its filter.
fn drawn(quads: &[Quad]) -> Vec<(Option<u16>, Filter)> {
    quads
        .iter()
        .map(|q| match q.filter {
            Filter::Bga | Filter::BgaLayer | Filter::Movie { .. } => (Some(q.layer), q.filter),
            _ => {
                assert_eq!((q.uv, q.layer), (SOLID.uv, SOLID.layer));
                assert_eq!(&q.color[..3], &[0.0, 0.0, 0.0], "the placeholder is black");
                (None, q.filter)
            }
        })
        .collect()
}

const PLACEHOLDER: (Option<u16>, Filter) = (None, Filter::Linear);
const fn base(i: u16) -> (Option<u16>, Filter) {
    (Some(i), Filter::Bga)
}
const fn layer(i: u16) -> (Option<u16>, Filter) {
    (Some(i), Filter::BgaLayer)
}

#[test]
fn base_image_follows_chart_time() {
    let mut h = harness(FILL, table());
    // A display offset far ahead: the BGA keeps to chart time.
    h.offset = TimeUs(1_500 * MS);
    let mut at = |ms: i64| {
        h.frame(ms, &[]);
        drawn(h.batch.quads())
    };
    assert_eq!(at(-1), [PLACEHOLDER], "before chart time 0");
    assert_eq!(at(0), [base(0)]);
    assert_eq!(at(499), [base(0)]);
    assert_eq!(at(500), [base(0), layer(2)]);
    assert_eq!(at(999), [base(0), layer(2)]);
    assert_eq!(at(1_000), [base(1), layer(2)]);
    // A cleared base shows the placeholder, the layer over it.
    assert_eq!(at(2_000), [PLACEHOLDER, layer(2)]);
    // Back in time (a rearm's new origin): placed again.
    assert_eq!(at(600), [base(0), layer(2)]);
}

#[test]
fn a_table_not_drawable_or_a_chart_without_bga_shows_the_placeholder() {
    let mut h = harness(FILL, table());
    h.play.set_bga_live(Some(BgaKey {
        renderer: 1,
        array: 8,
    }));
    h.frame(600, &[]);
    assert_eq!(drawn(h.batch.quads()), [PLACEHOLDER]);
    let mut none = Harness::new(skin(FILL), &chart(), &full(1_000));
    none.play.set_bga_images(Arc::new(table()));
    none.play.set_bga_live(Some(KEY));
    none.frame(600, &[]);
    assert_eq!(drawn(none.batch.quads()), [PLACEHOLDER]);
}

/// Inside a poor window: a table of an array no longer in use, or none
/// published yet, shows the placeholder, where a table published with no
/// array draws every picture absent, so the miss window draws nothing.
#[test]
fn an_unpublished_or_stale_table_shows_the_placeholder_in_a_poor_window() {
    let miss = |h: &mut Harness| {
        h.frame(
            3_200,
            &[ev(3_200, judged(1, 0, Judgment::Bad, Some(90), None, 0))],
        );
        drawn(h.batch.quads())
    };
    let mut drawable = harness(FILL, table());
    assert_eq!(miss(&mut drawable), [base(3)]);
    let mut stale = harness(FILL, table());
    stale.play.set_bga_live(Some(BgaKey {
        renderer: 1,
        array: 8,
    }));
    assert_eq!(miss(&mut stale), [PLACEHOLDER], "stale");
    // A lost device: no live array, the published table keyed to one.
    let mut lost = harness(FILL, table());
    lost.play.set_bga_live(None);
    assert_eq!(miss(&mut lost), [PLACEHOLDER], "no live array");
    let mut unpublished = harness(FILL, BgaTable::ABSENT);
    assert_eq!(miss(&mut unpublished), [PLACEHOLDER], "unpublished");
    let mut none = harness(FILL, BgaTable::new(vec![BgaEntry::Absent; 5]));
    none.play.set_bga_live(None);
    none.frame(600, &[]);
    assert_eq!(drawn(none.batch.quads()), [PLACEHOLDER], "base absent");
    assert_eq!(miss(&mut none), [], "published with no array");
}

#[test]
fn poor_replaces_base_and_layer_for_500ms_after_a_combo_break() {
    let mut h = harness(FILL, table());
    // A miss before the first poor sequence: base and layer keep drawing.
    h.frame(
        2_500,
        &[ev(2_500, judged(1, 0, Judgment::Poor, None, None, 0))],
    );
    assert_eq!(drawn(h.batch.quads()), [PLACEHOLDER, layer(2)]);
    h.frame(3_000, &[]);
    assert_eq!(drawn(h.batch.quads()), [PLACEHOLDER, layer(2)]);
    // A judgment keeping the combo: no miss window.
    h.frame(
        3_100,
        &[ev(3_100, judged(1, 1, Judgment::Great, Some(5), None, 3))],
    );
    assert_eq!(drawn(h.batch.quads()), [PLACEHOLDER, layer(2)]);
    // A combo break: the poor frames alone, each half of 500 ms.
    h.frame(
        3_200,
        &[ev(3_200, judged(1, 2, Judgment::Bad, Some(90), None, 0))],
    );
    assert_eq!(drawn(h.batch.quads()), [base(3)]);
    h.frame(3_449, &[]);
    assert_eq!(drawn(h.batch.quads()), [base(3)]);
    h.frame(3_450, &[]);
    assert_eq!(drawn(h.batch.quads()), [base(4)]);
    h.frame(3_699, &[]);
    assert_eq!(drawn(h.batch.quads()), [base(4)]);
    h.frame(3_700, &[]);
    assert_eq!(drawn(h.batch.quads()), [PLACEHOLDER, layer(2)]);
    // An empty POOR with the combo at 0 opens it too.
    h.frame(4_000, &[ev(4_000, empty_poor(1))]);
    assert_eq!(drawn(h.batch.quads()), [base(3)]);
    // A rearm forgets the miss.
    h.play.rearm();
    h.frame(4_100, &[]);
    assert_eq!(drawn(h.batch.quads()), [PLACEHOLDER, layer(2)]);
}

#[test]
fn absent_pictures_draw_per_channel_in_the_model() {
    // Base 0 and layer 2 and poor 3 absent: the base draws the
    // placeholder, the layer nothing, the miss window nothing.
    let mut entries: Vec<BgaEntry> = (0..5).map(|i| cell(i, [256, 256])).collect();
    entries[0] = BgaEntry::Absent;
    entries[2] = BgaEntry::Absent;
    entries[3] = BgaEntry::Video;
    let mut h = harness(FILL, BgaTable::new(entries).keyed(KEY));
    h.frame(600, &[]);
    assert_eq!(drawn(h.batch.quads()), [PLACEHOLDER]);
    h.frame(
        3_100,
        &[ev(3_100, judged(1, 0, Judgment::Poor, None, None, 0))],
    );
    assert_eq!(drawn(h.batch.quads()), []);
}

#[test]
fn trimmed_stretch_3_crops_at_frame_time() {
    // Picture 0 is 512×256 drawn into a 512×512 destination: the height
    // fills it, the width is cropped to its middle half.
    let mut entries: Vec<BgaEntry> = (0..5).map(|i| cell(i, [256, 256])).collect();
    entries[0] = cell(0, [512, 256]);
    let mut h = harness(
        BgaStretch::Crop(Crop::Outer),
        BgaTable::new(entries).keyed(KEY),
    );
    h.frame(100, &[]);
    let q = h.batch.quads()[0];
    assert_eq!(q.rect, [100.0, 100.0, 512.0, 512.0]);
    assert_eq!(q.uv, [0.015625, 0.0, 0.046875, 0.0625]);
    // A square picture fills it whole.
    h.frame(1_100, &[]);
    let q = h.batch.quads()[0];
    assert_eq!(q.rect, [100.0, 100.0, 512.0, 512.0]);
    assert_eq!(q.uv, [0.0625, 0.0, 0.125, 0.0625]);
}

/// A play whose session does not show the BGA draws the placeholder for
/// every frame, the base's, the layer's and a poor window's, where the
/// same play shown draws them.
#[test]
fn a_play_not_showing_the_bga_draws_the_placeholder_throughout() {
    let run = |shown: bool| {
        let mut c = chart();
        c.bga = Some(bga());
        let ctx = PlayContext {
            bga_shown: shown,
            ..full(1_000)
        };
        let mut h = Harness::new(skin(FILL), &c, &ctx);
        h.play.set_bga_images(Arc::new(table()));
        h.play.set_bga_live(Some(KEY));
        let mut at = Vec::new();
        for ms in [0, 600, 1_000] {
            h.frame(ms, &[]);
            at.push(drawn(h.batch.quads()));
        }
        h.frame(
            3_200,
            &[ev(3_200, judged(1, 0, Judgment::Bad, Some(90), None, 0))],
        );
        at.push(drawn(h.batch.quads()));
        at
    };
    assert_eq!(
        run(true),
        [
            vec![base(0)],
            vec![base(0), layer(2)],
            vec![base(1), layer(2)],
            vec![base(3)],
        ]
    );
    assert_eq!(run(false), [[PLACEHOLDER]; 4]);
}

#[test]
fn only_a_play_skin_draws_the_bga() {
    use play_support::def;
    use skin_model::{DecisionDefs, LoadError, ResultDefs, SelectionDefs};
    let on = |t| {
        vec![ElementDef {
            timer: TimerRef::Builtin(t),
            ..bga_element(FILL)
        }]
    };
    let selection = Skin::new_selection(
        def(
            Vec::new(),
            white_sheet(),
            on(skin_model::selection_timer::SCREEN),
        ),
        SelectionDefs::default(),
    );
    let result = Skin::new_result(
        def(
            Vec::new(),
            white_sheet(),
            on(skin_model::result_timer::SCREEN),
        ),
        ResultDefs::default(),
    );
    let decision = Skin::new_decision(
        def(Vec::new(), white_sheet(), vec![bga_element(FILL)]),
        DecisionDefs::default(),
    );
    for (kind, refused) in [
        (ScreenKind::Selection, selection.err()),
        (ScreenKind::Result, result.err()),
        (ScreenKind::Decision, decision.err()),
    ] {
        assert!(
            matches!(refused, Some(LoadError::SourceKind { .. })),
            "{kind:?}: {refused:?}"
        );
    }
    assert!(load(Vec::new(), vec![bga_element(FILL)]).is_ok());
}

/// A video's frame in movie slot `slot`, `size` pixels, BT.601 and full
/// range.
fn video_frame(slot: u16, size: [u32; 2]) -> MovieShow {
    MovieShow::Frame {
        uv: [0.0, 0.0, 1.0, 1.0],
        layer: slot,
        size,
        bt601: true,
        full: true,
    }
}

const MOVIE: Filter = Filter::Movie {
    bt601: true,
    full: true,
};
const fn video(slot: u16) -> (Option<u16>, Filter) {
    (Some(slot), MOVIE)
}

/// Shows of slots 2 and 3, the skin's slots nothing.
fn shows(two: MovieShow, three: MovieShow) -> MovieShows {
    [MovieShow::Nothing, MovieShow::Nothing, two, three]
}

/// The table with base 0, layer 2 and poor 3 videos, the rest pictures.
fn video_table() -> BgaTable {
    let mut entries: Vec<BgaEntry> = (0..5).map(|i| cell(i, [256, 256])).collect();
    for i in [0, 2, 3] {
        entries[i] = BgaEntry::Video;
    }
    BgaTable::new(entries).keyed(KEY)
}

/// Base 0 and poor 3 in slot 2, layer 2 in slot 3.
fn video_slots() -> ChartVideos {
    ChartVideos::new(&[Some(2), None, Some(3), Some(2), None])
}

/// A combo break at `ms`: the poor window from then.
fn miss(h: &mut Harness, ms: i64) -> Vec<(Option<u16>, Filter)> {
    h.frame(
        ms,
        &[ev(ms, judged(1, 0, Judgment::Bad, Some(90), None, 0))],
    );
    drawn(h.batch.quads())
}

/// A video id whose slot shows nothing draws as its channel's absence:
/// the base the placeholder, the layer and the poor frame nothing.
#[test]
fn a_video_with_nothing_to_show_draws_as_absent_per_channel() {
    let mut h = harness(FILL, video_table());
    h.play.set_chart_videos(video_slots());
    h.play
        .set_movies(shows(MovieShow::Nothing, MovieShow::Nothing));
    h.frame(600, &[]);
    assert_eq!(drawn(h.batch.quads()), [PLACEHOLDER], "base and layer");
    assert_eq!(miss(&mut h, 3_200), [], "the poor frame");
    // With frames, each channel draws its slot's: the base and the poor
    // frame slot 2's, the layer slot 3's.
    let mut h = harness(FILL, video_table());
    h.play.set_chart_videos(video_slots());
    h.play.set_movies(shows(
        video_frame(2, [256, 256]),
        video_frame(3, [256, 256]),
    ));
    h.frame(600, &[]);
    assert_eq!(drawn(h.batch.quads()), [video(2), video(3)]);
    assert_eq!(h.batch.movies(), [false, false, true, true]);
}

/// A poor frame naming a video draws its slot's frame as a movie quad,
/// sampled bilinearly, in place of the base and the layer.
#[test]
fn a_poor_frame_naming_a_video_draws_its_slot_s_frame() {
    let mut entries: Vec<BgaEntry> = (0..5).map(|i| cell(i, [256, 256])).collect();
    entries[3] = BgaEntry::Video;
    let mut h = harness(FILL, BgaTable::new(entries).keyed(KEY));
    h.play
        .set_chart_videos(ChartVideos::new(&[None, None, None, Some(3), None]));
    h.play
        .set_movies(shows(MovieShow::Nothing, video_frame(3, [256, 256])));
    assert_eq!(miss(&mut h, 3_200), [video(3)]);
    assert_eq!(h.batch.movies(), [false, false, false, true]);
    // The poor sequence's next frame, a picture.
    h.frame(3_450, &[]);
    assert_eq!(drawn(h.batch.quads()), [base(4)]);
}

/// A video is placed by the element's stretch against the video's own
/// size: a 640×360 video under stretch 3 in a square destination fills
/// its height and keeps the middle 9/16 of its width.
#[test]
fn a_video_is_placed_by_the_stretch_against_its_size() {
    let mut h = harness(BgaStretch::Crop(Crop::Outer), video_table());
    h.play.set_chart_videos(video_slots());
    h.play
        .set_movies(shows(video_frame(2, [640, 360]), MovieShow::Nothing));
    h.frame(100, &[]);
    let q = h.batch.quads()[0];
    assert_eq!(q.filter, MOVIE);
    assert_eq!(q.rect, [100.0, 100.0, 512.0, 512.0]);
    assert_eq!(q.uv, [0.21875, 0.0, 0.78125, 1.0]);
}

/// Without chart video slots set, as before chart videos play, every
/// video id draws as absent on each channel, whatever the movie slots
/// show.
#[test]
fn without_chart_videos_every_video_id_is_absent() {
    let mut h = harness(FILL, video_table());
    h.play.set_movies(shows(
        video_frame(2, [256, 256]),
        video_frame(3, [256, 256]),
    ));
    h.frame(600, &[]);
    assert_eq!(drawn(h.batch.quads()), [PLACEHOLDER]);
    assert_eq!(miss(&mut h, 3_200), []);
    assert_eq!(h.batch.movies(), [false; 4]);
}

/// While a chart video is lost, each `bga` element draws exactly one
/// opaque black quad over its rect, alpha-blended whatever its own blend
/// and colour, and nothing else; a skin movie lost does not black the
/// panel.
#[test]
fn a_lost_panel_draws_one_black_quad_per_element() {
    let second = ElementDef {
        blend: skin_model::Blend::Add,
        track: rect([700, 100, 300, 300]),
        ..bga_element(FILL)
    };
    let skin = load(Vec::new(), vec![bga_element(FILL), second]).unwrap();
    let mut c = chart();
    c.bga = Some(bga());
    let mut h = Harness::new(skin, &c, &full(1_000));
    h.play.set_bga_images(Arc::new(table()));
    h.play.set_bga_live(Some(KEY));
    h.play.set_movies([
        MovieShow::Lost,
        MovieShow::Nothing,
        MovieShow::Nothing,
        MovieShow::Nothing,
    ]);
    h.frame(600, &[]);
    assert_eq!(
        drawn(h.batch.quads()),
        [base(0), layer(2), base(0), layer(2)]
    );
    for lost in [
        shows(MovieShow::Lost, MovieShow::Nothing),
        shows(video_frame(2, [256, 256]), MovieShow::Lost),
    ] {
        h.play.set_movies(lost);
        h.frame(600, &[]);
        let quads = h.batch.quads();
        assert_eq!(drawn(quads), [PLACEHOLDER, PLACEHOLDER], "{lost:?}");
        assert_eq!(quads[0].rect, [100.0, 100.0, 512.0, 512.0]);
        assert_eq!(quads[1].rect, [700.0, 100.0, 300.0, 300.0]);
        for q in quads {
            assert_eq!(q.color, [0.0, 0.0, 0.0, 1.0], "opaque black");
        }
        let blends: Vec<_> = h.batch.runs().iter().map(|r| r.key.blend).collect();
        assert!(
            blends.iter().all(|b| *b == skin_model::Blend::Alpha),
            "{blends:?}"
        );
        assert_eq!(h.batch.movies(), [false; 4]);
    }
}

/// A play opens at most two video files, ranked by their first base or
/// layer start, a tie broken by channel then by the lowest id: aliases of
/// one file share its slot and its starts, each start once; a third file
/// and one only the poor sequences name take no slot.
// Source: SF-026
#[test]
fn chart_files_are_ranked_by_their_first_start_and_capped_at_two() {
    // Ids 0 and 1 name file 0, id 2 file 1, id 3 file 2, id 4 file 3 (poor
    // only), id 5 a picture.
    let file_of = [Some(0), Some(0), Some(1), Some(2), Some(3), None];
    let bga = ChartBga::new(
        (1..=6).map(image).collect(),
        vec![event(1_000, Some(3)), event(2_000, Some(0))],
        // File 1 ties file 2 at 1 000 ms on the layer, by a lower id: the
        // base wins.
        vec![
            event(500, Some(1)),
            event(1_000, Some(2)),
            event(2_000, Some(1)),
        ],
        vec![PoorSequence {
            time: TimeUs(3_000 * MS),
            frames: vec![Some(BgaImageId(4)), Some(BgaImageId(3))].into_boxed_slice(),
        }],
    )
    .unwrap();
    let (slots, files) = ChartVideos::choose(&bga, &file_of);
    assert_eq!(slots, [Some(2), Some(2), None, Some(3), None, None]);
    assert_eq!(files, [Some(0), Some(2)]);
    let videos = ChartVideos::of(&bga, &slots);
    let us = |ms: &[i64]| ms.iter().map(|m| TimeUs(m * MS)).collect::<Vec<_>>();
    assert_eq!(videos.starts(2), us(&[500, 2_000]), "aliases' starts, once");
    assert_eq!(videos.starts(3), us(&[1_000]));
    assert_eq!(videos.starts(0), us(&[]), "a skin slot has none");
    // The same file named first by the lowest id breaks a full tie.
    let tie = ChartBga::new(
        (1..=2).map(image).collect(),
        vec![event(0, Some(1)), event(0, Some(0))],
        vec![],
        vec![],
    )
    .unwrap();
    let (slots, files) = ChartVideos::choose(&tie, &[Some(7), Some(8)]);
    assert_eq!((slots, files), (vec![Some(2), Some(3)], [Some(7), Some(8)]));
}
