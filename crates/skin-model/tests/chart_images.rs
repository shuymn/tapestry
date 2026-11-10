//! The chart's own images in a play skin (S10-12): a chart image element
//! draws its installed uv, met by its stretch against the image's native
//! size, nothing while absent and nothing while unavailable; the fields
//! `chart.stagefile` and `chart.backbmp` read what is drawable, and while
//! loading, no condition reading them, negated or not, is evaluated until
//! the images are published; the atlas keeps a chart layer after the text
//! layer for a skin that draws one.

mod lanes_support;
mod play_support;
mod widget_support;

use bms_model::TimeUs;
use skin_model::chart_image::{ChartImage, ChartStretch, ChartTable, ChartUv};
use skin_model::{
    AtlasError, AtlasLimits, DecisionDefs, ElementDef, Filter, FrameUv, GUTTER, KeptLayers, Layout,
    LoadError, LoadState, Loading, LoadingFrame, MIN_TEXT_SIDE, Native, Place, QuadBatch,
    ResultDefs, ScreenKind, SelectionDefs, Skin, Source, TimerRef, UvTable, Variant, field, timer,
};

use lanes_support::rect;
use play_support::{Harness, VIEW, def, load, uvs, white_sheet};
use widget_support::{boolean, chart};

/// Where the stage file and the back image draw, and two marks: one while
/// the back image is drawable, one while it is not (the source's 195 and
/// 194).
const STAGE: [i32; 4] = [100, 100, 400, 300];
const BACK: [i32; 4] = [600, 100, 400, 300];
const BACK_MARK: [i32; 4] = [1_100, 100, 50, 50];
const NO_BACK_MARK: [i32; 4] = [1_200, 100, 50, 50];

/// The uvs the tests install: each image's own, on a chart layer 9.
fn stage_uv(native: [u32; 2]) -> ChartUv {
    ChartUv {
        uv: FrameUv {
            uv: [0.0, 0.0, 0.25, 0.25],
            layer: 9,
        },
        native,
    }
}

fn back_uv(native: [u32; 2]) -> ChartUv {
    ChartUv {
        uv: FrameUv {
            uv: [0.5, 0.0, 0.75, 0.25],
            layer: 9,
        },
        native,
    }
}

/// A chart image element on `screen`, so loading frames draw it too.
fn chart_image(which: ChartImage, stretch: ChartStretch, r: [i32; 4]) -> ElementDef {
    ElementDef {
        source: Source::ChartImage {
            which,
            filter: Filter::Linear,
            stretch,
        },
        track: rect(r),
        timer: TimerRef::Builtin(timer::SCREEN),
        visible: None,
        blend: skin_model::Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    }
}

/// Image 0 at `r` on `screen` while `visible`.
fn mark(r: [i32; 4], visible: &str) -> ElementDef {
    ElementDef {
        source: Source::Image(0),
        visible: Some(boolean(visible)),
        ..chart_image(ChartImage::Stage, ChartStretch::Fill, r)
    }
}

/// The skin: both images filling their rects, and the two marks, the
/// second through a negation.
fn skin() -> Skin {
    load(
        Vec::new(),
        vec![
            chart_image(ChartImage::Stage, ChartStretch::Fill, STAGE),
            chart_image(ChartImage::Back, ChartStretch::Fill, BACK),
            mark(BACK_MARK, "chart.backbmp"),
            mark(NO_BACK_MARK, "!chart.backbmp"),
        ],
    )
    .unwrap()
}

/// The rects the batch drew, in draw order, by the uv each drew.
fn drawn(batch: &QuadBatch) -> Vec<([f32; 4], [f32; 4])> {
    batch.quads().iter().map(|q| (q.rect, q.uv)).collect()
}

fn f(r: [i32; 4]) -> [f32; 4] {
    r.map(|v| v as f32)
}

/// A skin's loading state and the frame it builds with.
struct Loader {
    skin: Skin,
    uvs: UvTable,
    loading: Loading,
    batch: QuadBatch,
}

impl Loader {
    fn new(skin: Skin) -> Self {
        Self {
            uvs: uvs(&skin),
            loading: skin.prepare_loading(skin_model::Side::Left),
            batch: QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity()),
            skin,
        }
    }

    fn build(&mut self) {
        self.batch.clear();
        let layout = Layout::AUTHOR;
        let frame = LoadingFrame {
            difficulty: None,
            bga: None,
            autoplay: false,
            now: TimeUs(100_000),
            load: LoadState {
                since: TimeUs(0),
                percent: 0,
            },
            target: [1_920, 1_080],
            layout: &layout,
            turntable: 0,
            lanes: VIEW,
            covers: play_support::readouts_for(VIEW).covers,
            gauge: None,
            play_level: None,
        };
        self.skin
            .build_loading(&mut self.loading, &frame, &self.uvs, &mut self.batch)
            .unwrap();
    }

    fn field(&self, slot: u16) -> i32 {
        self.loading.state().fields[usize::from(slot)]
    }
}

/// While loading, the images and every condition reading their fields,
/// plain or negated, draw nothing until the images are published; then a
/// present image draws its uv and an absent one nothing, and the fields
/// read what is drawable. Unavailable again, nothing draws again.
#[test]
fn loading_frames_wait_for_the_images_to_be_published() {
    let mut l = Loader::new(skin());
    l.build();
    assert_eq!(drawn(&l.batch), Vec::new(), "unavailable");

    let back = back_uv([640, 480]);
    l.loading
        .set_chart_images(Some(ChartTable::new(None, Some(back))));
    l.build();
    assert_eq!(
        drawn(&l.batch),
        vec![(f(BACK), back.uv.uv), (f(BACK_MARK), drawn(&l.batch)[1].1)]
    );
    assert_eq!(l.batch.quads()[0].layer, 9);
    assert_eq!(
        (
            l.field(field::CHART_STAGEFILE),
            l.field(field::CHART_BACKBMP)
        ),
        (0, 1)
    );

    l.loading.set_chart_images(Some(ChartTable::ABSENT));
    l.build();
    let rects: Vec<_> = drawn(&l.batch).into_iter().map(|(r, _)| r).collect();
    assert_eq!(rects, vec![f(NO_BACK_MARK)], "absent: the negation holds");
    assert_eq!(l.field(field::CHART_BACKBMP), 0);

    l.loading.set_chart_images(None);
    l.build();
    assert_eq!(drawn(&l.batch), Vec::new(), "unavailable again");
}

/// A loading state's reset for another lifetime keeps the images: they
/// belong to the installation.
#[test]
fn a_reset_keeps_the_published_images() {
    let mut l = Loader::new(skin());
    let table = ChartTable::new(Some(stage_uv([640, 480])), None);
    l.loading.set_chart_images(Some(table));
    l.loading.reset();
    assert_eq!(l.loading.chart_images(), Some(&table));
}

/// A play draws the images set for it, and its fields read them; with
/// none set it draws neither and its negations hold.
#[test]
fn a_play_draws_its_images_and_reads_their_fields() {
    let mut h = Harness::new(skin(), &chart(), &play_support::full(0));
    h.frame(0, &[]);
    let rects: Vec<_> = drawn(&h.batch).into_iter().map(|(r, _)| r).collect();
    assert_eq!(rects, vec![f(NO_BACK_MARK)], "absent until set");
    assert_eq!(h.field(field::CHART_STAGEFILE), 0);

    let (stage, back) = (stage_uv([640, 480]), back_uv([640, 480]));
    h.play
        .set_chart_images(ChartTable::new(Some(stage), Some(back)));
    h.frame(10, &[]);
    let got = drawn(&h.batch);
    assert_eq!(got[0], (f(STAGE), stage.uv.uv));
    assert_eq!(got[1], (f(BACK), back.uv.uv));
    assert_eq!(got[2].0, f(BACK_MARK));
    assert_eq!(got.len(), 3);
    assert_eq!(
        (
            h.field(field::CHART_STAGEFILE),
            h.field(field::CHART_BACKBMP)
        ),
        (1, 1)
    );
    // A rearm keeps them.
    h.play.rearm();
    h.frame(20, &[]);
    assert_eq!(drawn(&h.batch).len(), 3);
}

/// Each stretch meets the destination against the native size as the
/// source's `StretchType` does; 8 and 9 keep the native size at placement.
#[test]
fn stretches_meet_the_native_size() {
    let r = [100.0, 100.0, 400.0, 100.0];
    // 2 × 1 in a 4 × 1 box: inner fits the height, outer the width.
    let n = [200, 100];
    let place = |s: ChartStretch| s.place(r, n);
    assert_eq!(place(ChartStretch::Fill), (r, None));
    assert_eq!(
        place(ChartStretch::Inner),
        ([200.0, 100.0, 200.0, 100.0], None)
    );
    assert_eq!(
        place(ChartStretch::Outer),
        ([100.0, 50.0, 400.0, 200.0], None)
    );
    assert_eq!(
        place(ChartStretch::FitWidth),
        ([100.0, 50.0, 400.0, 200.0], None)
    );
    assert_eq!(
        place(ChartStretch::FitHeight),
        ([200.0, 100.0, 200.0, 100.0], None)
    );
    assert_eq!(
        place(ChartStretch::NoExpanding),
        (r, Some(Native::NoLarger(n)))
    );
    assert_eq!(place(ChartStretch::NoResize), (r, Some(Native::Exact(n))));
    // 1 × 2 in the same box: inner fits the width, outer the height.
    let tall = [100, 200];
    assert_eq!(
        ChartStretch::Inner.place(r, tall),
        ([275.0, 100.0, 50.0, 100.0], None)
    );
    assert_eq!(
        ChartStretch::Outer.place(r, tall),
        ([100.0, -250.0, 400.0, 800.0], None)
    );
    // An image without area keeps the destination.
    assert_eq!(ChartStretch::Inner.place(r, [0, 10]), (r, None));
}

/// A stretched chart image draws its stretched rect.
#[test]
fn a_chart_image_draws_its_stretched_rect() {
    let s = load(
        Vec::new(),
        vec![chart_image(
            ChartImage::Stage,
            ChartStretch::Inner,
            [100, 100, 400, 100],
        )],
    )
    .unwrap();
    let mut h = Harness::new(s, &chart(), &play_support::full(0));
    h.play
        .set_chart_images(ChartTable::new(Some(stage_uv([200, 100])), None));
    h.frame(0, &[]);
    assert_eq!(drawn(&h.batch)[0].0, [200.0, 100.0, 200.0, 100.0]);
}

/// A chart image is play's: the other screens refuse it, and a play skin
/// drawing one keeps a chart layer, counted after the text layer.
#[test]
fn a_chart_image_is_plays_and_keeps_a_chart_layer() {
    let element = || chart_image(ChartImage::Stage, ChartStretch::Fill, STAGE);
    let selection = Skin::new_selection(
        def(Vec::new(), white_sheet(), vec![element()]),
        SelectionDefs::default(),
    );
    assert!(matches!(
        selection,
        Err(LoadError::SourceKind {
            kind: ScreenKind::Selection,
            ..
        })
    ));
    let result = Skin::new_result(
        def(Vec::new(), white_sheet(), vec![element()]),
        ResultDefs::default(),
    );
    assert!(matches!(result, Err(LoadError::SourceKind { .. })));
    let decision = Skin::new_decision(
        def(Vec::new(), white_sheet(), vec![element()]),
        DecisionDefs::default(),
    );
    assert!(matches!(decision, Err(LoadError::SourceKind { .. })));

    let plain = load(Vec::new(), Vec::new()).unwrap();
    assert!(!plain.has_chart_images());
    assert_eq!(plain.kept_layers(), KeptLayers::NONE);
    let s = skin();
    assert!(s.has_chart_images());
    assert_eq!(
        s.kept_layers(),
        KeptLayers {
            text: false,
            chart: true
        }
    );
    let limits = AtlasLimits {
        max_side: 8_192,
        max_layers: 16,
        reserved_layers: 1,
    };
    let atlas = s.atlas(Variant::Lo, limits, &|| false).unwrap();
    assert_eq!(
        atlas.side(),
        MIN_TEXT_SIDE,
        "a chart layer alone sets the least side"
    );
    assert_eq!(atlas.text_layer(), None);
    let pages = u16::try_from(atlas.pages().len()).unwrap();
    assert_eq!(atlas.chart_layer(), Some(1 + pages));
    let side = u64::from(MIN_TEXT_SIDE);
    assert_eq!(
        s.sheet()
            .plan_keeping(Variant::Lo, limits, s.kept_layers())
            .unwrap(),
        (1 + u64::from(pages) + 1) * side * side
    );
    // Both: the chart layer after the text layer.
    let both = KeptLayers {
        text: true,
        chart: true,
    };
    let a = s
        .sheet()
        .atlas_keeping(Variant::Lo, limits, both, &|| false)
        .unwrap();
    assert_eq!(a.text_layer(), Some(1 + pages));
    assert_eq!(a.chart_layer(), Some(2 + pages));
    assert_eq!(
        s.sheet().plan_keeping(Variant::Lo, limits, both).unwrap(),
        (1 + u64::from(pages) + 2) * side * side
    );
    // A rebind must name the layers the atlas keeps.
    assert!(matches!(
        a.rebind(s.sheet(), Variant::Lo, true),
        Err(AtlasError::NotHeld)
    ));
    assert!(a.rebind_keeping(s.sheet(), Variant::Lo, both).is_ok());
    // A device without a layer for it refuses.
    let tight = AtlasLimits {
        max_layers: 1 + pages as u32 + 1,
        ..limits
    };
    assert!(
        s.sheet()
            .atlas_keeping(Variant::Lo, tight, both, &|| false)
            .is_err()
    );
}

/// A slot is half the side square, the stage file's at the top left and
/// the back image's at the top right; a uv covers the content only.
#[test]
fn slots_and_uvs_cover_the_content_only() {
    let (origin, slot) = ChartImage::Back.slot(2_048).unwrap();
    assert_eq!((origin, slot.size()), ([1_024, 0], [1_024, 1_024]));
    assert_eq!(ChartImage::Stage.slot(2_048).unwrap().0, [0, 0]);
    assert!(ChartImage::Stage.slot(40).is_none());

    let d = tempfile::tempdir().unwrap();
    image::RgbaImage::from_pixel(5, 3, image::Rgba([9, 9, 9, 255]))
        .save(d.path().join("s.png"))
        .unwrap();
    let dir = skin_model::chart_image::ChartDir::new(d.path()).unwrap();
    let read = skin_model::chart_image::read(&dir, "s.png", &[], slot, &|| false, &|_| None);
    let fitted = read.image.unwrap();
    let uv = ChartUv::of(&fitted, ChartImage::Back, 2_048, 7).unwrap();
    let at = |v: u32| v as f32 / 2_048.0;
    assert_eq!(
        uv.uv.uv,
        [
            at(1_024 + GUTTER),
            at(GUTTER),
            at(1_024 + GUTTER + 5),
            at(GUTTER + 3)
        ]
    );
    assert_eq!((uv.uv.layer, uv.native), (7, [5, 3]));
    // A fit for a larger slot does not fit a smaller one.
    let big = image::RgbaImage::from_pixel(1_500, 10, image::Rgba([9; 4]));
    big.save(d.path().join("b.png")).unwrap();
    let wide = skin_model::chart_image::read(
        &dir,
        "b.png",
        &[],
        ChartImage::Stage.slot(4_096).unwrap().1,
        &|| false,
        &|_| None,
    )
    .image
    .unwrap();
    assert!(ChartUv::of(&wide, ChartImage::Stage, 2_048, 7).is_none());
}
