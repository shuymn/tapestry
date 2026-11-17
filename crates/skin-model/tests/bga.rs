//! The chart's BGA in a play skin (design.md "Rendering and Skins", BGA):
//! a `bga` element draws the base and the layer in effect at the frame's
//! chart time, never the display time; the poor pictures replace both for
//! 500 ms after a judgment that leaves the combo at 0, once a poor
//! sequence has passed; the black placeholder before chart time 0, for a
//! base with no picture and while the table is not drawable; a cropping
//! stretch crops at each frame against the picture's drawn size; and only
//! a play skin may draw it.

mod lanes_support;
mod play_support;
mod widget_support;

use std::sync::Arc;

use bms_model::{BgaEvent, BgaImage, BgaImageId, ChartBga, PoorSequence, TimeUs};
use rules::Judgment;
use skin_model::bga::{BgaCell, BgaEntry, BgaKey, BgaStretch, BgaTable, Crop, SOLID};
use skin_model::chart_image::ChartStretch;
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
            Filter::Bga | Filter::BgaLayer => (Some(q.layer), q.filter),
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
