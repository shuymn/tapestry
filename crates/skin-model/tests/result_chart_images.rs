//! The chart's own images on a result frame (S10-12): a chart image
//! element draws the view's table, keyed to the array its staging wrote
//! (nothing with no table, an absent one, or another array's), its
//! stretch met against the image's native size; `chart.stagefile` and
//! `chart.backbmp` read the attempt's resolved availability, which a view
//! carries, under every status.

mod result_support;
mod selection_support;

use skin_model::chart_image::{ChartImage, ChartKey, ChartStretch, ChartTable, ChartUv};
use skin_model::{Filter, FrameUv, ImageFacts, Source, result_field as field};

use result_support::{RFx, ready, relement, rload};

const KEY: ChartKey = ChartKey {
    renderer: 1,
    installation: 2,
    atlas: 3,
    generation: 4,
};

/// A 1 600×1 200 stage file on chart layer 9.
const STAGE: ChartUv = ChartUv {
    uv: FrameUv {
        uv: [0.0, 0.0, 0.390_625, 0.292_968_75],
        layer: 9,
    },
    native: [1_600, 1_200],
};

/// The stage file, stretch 1, in a 400×150 box at (100, 700); a mark at
/// y 50 while the stage file is not an image (190).
fn skin() -> skin_model::Skin {
    rload(
        vec![
            relement(
                Source::ChartImage {
                    which: ChartImage::Stage,
                    filter: Filter::Linear,
                    stretch: ChartStretch::Inner,
                },
                [100, 700, 400, 150],
                None,
            ),
            relement(Source::Image(0), [0, 50, 8, 8], Some("!chart.stagefile")),
        ],
        skin_model::WidgetDefs::default(),
    )
    .unwrap()
}

/// The view's chart images draw at their placement while the table is of
/// the live array; nothing with it absent or of another array. 190 holds
/// only while the view says the stage file is not an image, whatever the
/// table.
#[test]
fn a_result_frame_draws_the_views_chart_images() {
    let mut fx = RFx::new(skin());
    let mut v = ready(1_234, skin_model::BestView::Unavailable, 3);
    v.chart.images = ImageFacts {
        stage: true,
        back: false,
    };
    fx.view(&v);
    assert_eq!(
        fx.res.state().fields[usize::from(field::CHART_STAGEFILE)],
        1
    );
    assert_eq!(fx.res.state().fields[usize::from(field::CHART_BACKBMP)], 0);
    let stage = |fx: &RFx| -> Vec<[f32; 4]> {
        fx.batch
            .quads()
            .iter()
            .filter(|q| q.uv == STAGE.uv.uv && q.layer == 9)
            .map(|q| q.rect)
            .collect()
    };
    let mark = |fx: &RFx| fx.batch.quads().iter().any(|q| q.rect[1] == 50.0);
    fx.frame(0);
    assert!(stage(&fx).is_empty(), "no table: nothing");
    fx.res.set_chart_live(Some(KEY));
    fx.res
        .set_chart_images(ChartTable::new(Some(STAGE), None).keyed(KEY));
    fx.frame(10);
    let want = ChartStretch::Inner
        .place([100.0, 700.0, 400.0, 150.0], STAGE.native)
        .0;
    assert_eq!(stage(&fx), [want]);
    assert!(!mark(&fx), "190 reads the view's availability");
    // Another array's table draws nothing.
    fx.res.set_chart_live(Some(ChartKey {
        generation: 5,
        ..KEY
    }));
    fx.frame(20);
    assert!(stage(&fx).is_empty());
    // An absent one neither; a view without the stage file marks 190.
    fx.res.set_chart_live(Some(KEY));
    fx.res.set_chart_images(ChartTable::ABSENT.keyed(KEY));
    v.chart.images = ImageFacts::default();
    fx.view(&v);
    fx.frame(30);
    assert!(stage(&fx).is_empty());
    assert!(mark(&fx));
}
