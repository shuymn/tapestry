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
use skin_model::chart_image::{ChartImage, ChartKey, ChartStretch, ChartTable, ChartUv};

/// The array the tests' tables describe, and the frames' live key.
const KEY: ChartKey = ChartKey {
    renderer: 1,
    installation: 2,
    atlas: 3,
    generation: 4,
};

/// A table of `stage` and `back` describing [`KEY`]'s array.
fn table(stage: Option<ChartUv>, back: Option<ChartUv>) -> ChartTable {
    ChartTable::new(stage, back).keyed(KEY)
}
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
            loading: {
                let mut l = skin.prepare_loading(skin_model::Side::Left);
                l.set_chart_live(Some(KEY));
                l
            },
            batch: QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity()),
            skin,
        }
    }

    fn build(&mut self) {
        self.batch.clear();
        let layout = Layout::AUTHOR;
        let frame = LoadingFrame {
            app: skin_model::AppReadouts::LAUNCH,
            display_offset: TimeUs(0),
            fix_hispeed: 0,
            difficulty: None,
            bga: None,
            length: None,
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
    l.loading.set_chart_images(Some(table(None, Some(back))));
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

    l.loading
        .set_chart_images(Some(ChartTable::ABSENT.keyed(KEY)));
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
    let table = table(Some(stage_uv([640, 480])), None);
    l.loading.set_chart_images(Some(table));
    l.loading.reset();
    assert_eq!(l.loading.chart_images(), Some(&table));
}

/// A play draws the images set for it, and its fields read them; with
/// none set it draws neither and its negations hold.
#[test]
fn a_play_draws_its_images_and_reads_their_fields() {
    let mut h = Harness::new(skin(), &chart(), &play_support::full(0));
    h.play.set_chart_live(Some(KEY));
    h.frame(0, &[]);
    let rects: Vec<_> = drawn(&h.batch).into_iter().map(|(r, _)| r).collect();
    assert_eq!(rects, vec![f(NO_BACK_MARK)], "absent until set");
    assert_eq!(h.field(field::CHART_STAGEFILE), 0);

    let (stage, back) = (stage_uv([640, 480]), back_uv([640, 480]));
    h.play.set_chart_images(table(Some(stage), Some(back)));
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
    h.play.set_chart_live(Some(KEY));
    h.play
        .set_chart_images(table(Some(stage_uv([200, 100])), None));
    h.frame(0, &[]);
    assert_eq!(drawn(&h.batch)[0].0, [200.0, 100.0, 200.0, 100.0]);
}

/// A chart image is play's and the result's: selection and decision
/// refuse it, a result skin drawing one keeps a chart layer, and so does a
/// play skin, counted after the text layer.
#[test]
fn a_chart_image_is_play_and_results_and_keeps_a_chart_layer() {
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
        def(
            Vec::new(),
            white_sheet(),
            vec![ElementDef {
                timer: TimerRef::Builtin(skin_model::result_timer::SCREEN),
                ..element()
            }],
        ),
        ResultDefs::default(),
    )
    .unwrap();
    assert!(result.has_chart_images());
    assert!(result.kept_layers().chart);
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

/// A play skin that gives up its chart layer (an atlas that cannot hold
/// it) keeps none: its plan and atlas lose the layer and the least side
/// it set, and its chart-image elements stay, drawing nothing.
#[test]
fn a_skin_without_its_chart_layer_keeps_none() {
    let limits = AtlasLimits {
        max_side: 8_192,
        max_layers: 16,
        reserved_layers: 1,
    };
    let mut s = skin();
    let with = s
        .sheet()
        .plan_keeping(Variant::Lo, limits, s.kept_layers())
        .unwrap();
    s.without_chart_layer();
    assert!(!s.has_chart_images());
    assert_eq!(s.kept_layers(), KeptLayers::NONE);
    let atlas = s.atlas(Variant::Lo, limits, &|| false).unwrap();
    assert_eq!(atlas.chart_layer(), None);
    assert!(
        atlas.side() < MIN_TEXT_SIDE,
        "no kept layer sets the least side: {}",
        atlas.side()
    );
    let without = s
        .sheet()
        .plan_keeping(Variant::Lo, limits, s.kept_layers())
        .unwrap();
    assert!(without < with, "{without} < {with}");
    // The elements stay and draw nothing: the frame is the one the same
    // skin draws with no chart image published.
    let mut h = Harness::new(s, &chart(), &play_support::full(0));
    h.play.set_chart_live(Some(KEY));
    h.frame(0, &[]);
    let mut kept = Harness::new(skin(), &chart(), &play_support::full(0));
    kept.play.set_chart_live(Some(KEY));
    kept.frame(0, &[]);
    assert_eq!(drawn(&h.batch), drawn(&kept.batch));
}

/// A play skin of `cells` square images of `side` texels, each drawn,
/// each its own cut, with a chart image: its atlas needs a page per cell
/// at sides below twice `side`.
fn big_skin(cells: usize, side: u32) -> Skin {
    let sheet = skin_model::Sheet::new(
        play_support::white_root(),
        vec![skin_model::FileDef {
            path: "white.png".into(),
            hi: None,
        }],
        (0..cells)
            .map(|i| skin_model::ImageDef {
                file: 0,
                // Each its own cell: equal cuts would share one.
                rect: Some([0, 0, side + u32::try_from(i).unwrap(), side]),
                grid: [1, 1],
                cycle: None,
                filter: Filter::Linear,
            })
            .collect(),
    )
    .unwrap();
    let mut elements = vec![chart_image(ChartImage::Back, ChartStretch::Fill, BACK)];
    for i in 0..cells {
        elements.push(ElementDef {
            source: Source::Image(u32::try_from(i).unwrap()),
            ..mark(STAGE, "true")
        });
    }
    Skin::new(def(Vec::new(), sheet, elements)).unwrap()
}

/// The fallback keeps the chart layer when the atlas holds it, and gives
/// it up for each refusal it can matter to, when the plan without it fits:
/// the layers the device has (`Limits`), the pages they leave
/// (`TooManyPages`) and the per-atlas cap (`AtlasTooLarge`).
#[test]
fn the_chart_layer_is_given_up_only_where_it_does_not_fit() {
    let lo = Variant::Lo;
    // It fits: kept, nothing returned.
    let mut s = skin();
    let roomy = AtlasLimits {
        max_side: 8_192,
        max_layers: 16,
        reserved_layers: 1,
    };
    assert!(s.fit_chart_layer(lo, roomy).is_none());
    assert!(s.has_chart_images());

    // One page and the reserved layer: no layer left for the chart.
    let mut s = skin();
    let tight = AtlasLimits {
        max_layers: 2,
        ..roomy
    };
    assert!(matches!(
        s.fit_chart_layer(lo, tight),
        Some(AtlasError::Limits)
    ));
    assert!(!s.has_chart_images());
    assert!(s.atlas(lo, tight, &|| false).is_ok());

    // Three 1 500-texel cells at side 2 048: three pages. With the chart
    // layer the device's four layers leave two pages; without it, three.
    let mut s = big_skin(3, 1_500);
    let four = AtlasLimits {
        max_side: 2_048,
        max_layers: 4,
        reserved_layers: 1,
    };
    assert!(matches!(
        s.fit_chart_layer(lo, four),
        Some(AtlasError::TooManyPages)
    ));
    assert!(!s.has_chart_images());
    assert_eq!(
        s.sheet().plan_keeping(lo, four, s.kept_layers()).unwrap(),
        4 * 1_536 * 1_536,
        "three pages and the reserved layer, at a side the chart layer no longer raises"
    );

    // Three 8 000-texel cells, a page each: with the chart layer five
    // layers of about 64 Mi texels pass the 256 Mi cap; without it, four
    // stay within it.
    let mut s = big_skin(3, 8_000);
    let big = AtlasLimits {
        max_side: 8_192,
        max_layers: 16,
        reserved_layers: 1,
    };
    assert!(matches!(
        s.fit_chart_layer(lo, big),
        Some(AtlasError::AtlasTooLarge { .. })
    ));
    assert!(!s.has_chart_images());
    let without = s.sheet().plan_keeping(lo, big, s.kept_layers()).unwrap();
    assert!(without <= skin_model::MAX_ATLAS_TEXELS, "{without}");

    // A cell too large for any side: no fallback, the build refuses.
    let mut s = big_skin(1, 9_000);
    assert!(s.fit_chart_layer(lo, big).is_none());
    assert!(s.has_chart_images());
    assert!(matches!(
        s.atlas(lo, big, &|| false),
        Err(AtlasError::CellTooLarge { .. })
    ));
}

/// The plan's shape names the plan's own figures: its side, its pages,
/// every layer it charges and their texels.
#[cfg(feature = "dev-tools")]
#[test]
fn a_plans_shape_names_its_side_pages_and_layers() {
    let limits = AtlasLimits {
        max_side: 8_192,
        max_layers: 16,
        reserved_layers: 1,
    };
    let s = skin();
    let kept = KeptLayers {
        text: true,
        chart: true,
    };
    let shape = s.sheet().plan_shape(Variant::Lo, limits, kept).unwrap();
    assert_eq!(shape.side, MIN_TEXT_SIDE);
    assert_eq!(shape.layers, 1 + shape.pages + 2);
    assert_eq!(
        shape.texels,
        s.sheet().plan_keeping(Variant::Lo, limits, kept).unwrap()
    );
    assert_eq!(
        shape.texels,
        u64::from(shape.layers) * u64::from(shape.side) * u64::from(shape.side)
    );
    let none = s
        .sheet()
        .plan_shape(Variant::Lo, limits, KeptLayers::NONE)
        .unwrap();
    assert_eq!(none.layers, 1 + none.pages);
    assert!(none.side < shape.side);
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

/// A table draws only in a frame whose live key is its own: another
/// renderer's, installation's, atlas's or generation's table, an unkeyed
/// one, or any while the live key is unknown (a lost device) reads as
/// unavailable while loading and absent in a play, whatever was set.
#[test]
fn only_a_table_of_the_live_array_draws() {
    let others = [
        ChartKey { renderer: 9, ..KEY },
        ChartKey {
            installation: 9,
            ..KEY
        },
        ChartKey { atlas: 9, ..KEY },
        ChartKey {
            generation: 9,
            ..KEY
        },
    ];
    let back = back_uv([640, 480]);
    let mut l = Loader::new(skin());
    l.loading.set_chart_images(Some(table(None, Some(back))));
    l.build();
    assert!(!drawn(&l.batch).is_empty(), "its own array draws");
    for live in others.map(Some).into_iter().chain([None]) {
        l.loading.set_chart_live(live);
        l.build();
        assert_eq!(drawn(&l.batch), Vec::new(), "{live:?}: unavailable");
    }
    l.loading.set_chart_live(Some(KEY));
    l.loading
        .set_chart_images(Some(ChartTable::new(None, Some(back))));
    l.build();
    assert_eq!(drawn(&l.batch), Vec::new(), "an unkeyed table: unavailable");

    let mut h = Harness::new(skin(), &chart(), &play_support::full(0));
    h.play.set_chart_images(table(None, Some(back)));
    for live in others.map(Some).into_iter().chain([None]) {
        h.play.set_chart_live(live);
        h.frame(0, &[]);
        let rects: Vec<_> = drawn(&h.batch).into_iter().map(|(r, _)| r).collect();
        assert_eq!(rects, vec![f(NO_BACK_MARK)], "{live:?}: absent");
        assert_eq!(h.field(field::CHART_BACKBMP), 0);
    }
    h.play.set_chart_live(Some(KEY));
    h.frame(10, &[]);
    assert_eq!(h.field(field::CHART_BACKBMP), 1);
}

/// Placement is exact: a half-up decision is never moved by a float's
/// error. 25 × 25 in 7 × 8: the importer's exact top is 1/2, so 1.
#[test]
fn placement_rounds_exactly() {
    let r = [0.0, 0.0, 7.0, 8.0];
    assert_eq!(
        ChartStretch::Inner.place(r, [25, 25]).0,
        [0.0, 1.0, 7.0, 7.0]
    );
    assert_eq!(
        ChartStretch::Outer.place(r, [25, 25]).0,
        [0.0, 0.0, 8.0, 8.0]
    );
}

/// An animated destination is stretched and rounded at each sample: at
/// whole-pixel keys this is the importer's baked key; between keys the
/// model's quad sits on whole pixels where the importer's interpolated
/// baked keys sit on a fraction, and since a size animation changes the
/// stretch's own centring, by up to the distance between the two baked
/// keys' edges, not by half a pixel (design note D-chart-8). An `Inner`
/// destination moving x from 0 to 1, sampled a quarter of the way: the
/// model places it at 0, the baked keys at 0.25.
#[test]
fn an_animated_destination_rounds_at_each_sample() {
    let at_key = ChartStretch::Inner.place([1.0, 0.0, 7.0, 8.0], [25, 25]).0;
    assert_eq!(at_key, [1.0, 1.0, 7.0, 7.0]);
    let quarter = ChartStretch::Inner.place([0.25, 0.0, 7.0, 8.0], [25, 25]).0;
    assert_eq!(quarter, [0.0, 1.0, 7.0, 7.0]);
}

/// A size animation: a 2 × 1 image, `Inner`, its destination growing from
/// 2 × 2 at y = 0 to 3 × 2 (same top): at the keys the model places what
/// the importer bakes (top 1 then 0.5 rounded to 1, so `[0, 1, 2, 1]` and
/// `[0, 0, 3, 2]` after its own centring), and a quarter of the way it
/// places the sampled `[0, 0, 2.25, 2]` with top 0, where the baked keys'
/// interpolation would give 0.75. Today's behaviour, pinned (D-chart-8).
#[test]
fn a_size_animation_rounds_its_sample_not_its_keys() {
    let native = [2, 1];
    assert_eq!(
        ChartStretch::Inner.place([0.0, 0.0, 2.0, 2.0], native).0,
        [0.0, 1.0, 2.0, 1.0]
    );
    let quarter = ChartStretch::Inner.place([0.0, 0.0, 2.25, 2.0], native).0;
    assert_eq!(quarter[1], 0.0, "the sample's top, rounded");
}
