//! The cursor's chart's images on a selection frame (S10-12): a chart
//! image element draws the stage file or banner of the table set, keyed to
//! the selection installation's array (nothing with no table, an absent
//! one, or another array's), its stretch met against the image's native
//! size; `selected.stagefile` and `selected.banner` read the table set,
//! written after the view's fields, which never move the settings or the
//! application's readouts.

mod selection_support;

use skin_model::chart_image::{ChartImage, ChartKey, ChartStretch, ChartTable, ChartUv};
use skin_model::{Filter, FrameUv, SelectionDefs, Source, WidgetDefs, selection_field as field};

use selection_support::{Fx, boolean, chart, element, load, view};

const KEY: ChartKey = ChartKey {
    renderer: 1,
    installation: 2,
    atlas: 3,
    generation: 4,
};

/// A 640×480 stage file and a 300×80 banner on chart layer 9.
const STAGE: ChartUv = ChartUv {
    uv: FrameUv {
        uv: [0.0, 0.0, 0.156_25, 0.117_187_5],
        layer: 9,
    },
    native: [640, 480],
};
const BANNER: ChartUv = ChartUv {
    uv: FrameUv {
        uv: [0.0, 0.5, 0.073_242_19, 0.519_531_25],
        layer: 9,
    },
    native: [300, 80],
};

const STAGE_AT: [i32; 4] = [100, 300, 640, 480];
const BANNER_AT: [i32; 4] = [800, 100, 600, 160];

fn chart_image(which: ChartImage, stretch: ChartStretch, r: [i32; 4]) -> skin_model::ElementDef {
    element(
        Source::ChartImage {
            which,
            filter: Filter::Linear,
            stretch,
        },
        r,
    )
}

/// The stage file (stretch 4) and the banner (stretch 1), a mark at y 50
/// while the stage file is not drawable (190) and one at y 60 while the
/// banner is (193).
fn skin() -> Fx {
    let mark = |y: i32, when: &str| {
        let mut e = element(Source::Image(0), [0, y, 8, 8]);
        e.visible = Some(boolean(when));
        e
    };
    Fx::new(
        load(
            vec![
                chart_image(ChartImage::Stage, ChartStretch::Inner, STAGE_AT),
                chart_image(ChartImage::Banner, ChartStretch::Fill, BANNER_AT),
                mark(50, "!selected.stagefile"),
                mark(60, "selected.banner"),
            ],
            WidgetDefs::default(),
            SelectionDefs::default(),
        )
        .unwrap(),
    )
}

/// The rects drawn from `uv` on layer 9.
fn drawn(fx: &Fx, uv: ChartUv) -> Vec<[f32; 4]> {
    fx.batch
        .quads()
        .iter()
        .filter(|q| q.uv == uv.uv.uv && q.layer == 9)
        .map(|q| q.rect)
        .collect()
}

fn mark(fx: &Fx, y: f32) -> bool {
    fx.batch.quads().iter().any(|q| q.rect[1] == y)
}

fn fields(fx: &Fx) -> [i32; 2] {
    let f = fx.sel.state().fields;
    [
        f[usize::from(field::SELECTED_STAGEFILE)],
        f[usize::from(field::SELECTED_BANNER)],
    ]
}

fn place(stretch: ChartStretch, r: [i32; 4], native: [u32; 2]) -> [f32; 4] {
    stretch.place(r.map(|v| v as f32), native).0
}

/// The table's images draw at their placements while it is of the live
/// array; nothing with none set, an absent one or another array's.
#[test]
fn a_selection_frame_draws_the_cursors_chart_images() {
    let mut fx = skin();
    fx.view(&view(vec![chart("a")], Some(0)));
    fx.sel.set_chart_live(Some(KEY));
    fx.frame(0);
    assert!(drawn(&fx, STAGE).is_empty(), "no table: nothing");
    assert!(drawn(&fx, BANNER).is_empty());

    fx.sel.set_chart_images(Some(
        ChartTable::new(Some(STAGE), None, Some(BANNER)).keyed(KEY),
    ));
    fx.frame(10);
    assert_eq!(
        drawn(&fx, STAGE),
        [place(ChartStretch::Inner, STAGE_AT, STAGE.native)]
    );
    assert_eq!(
        drawn(&fx, BANNER),
        [place(ChartStretch::Fill, BANNER_AT, BANNER.native)]
    );
    // Another array's table draws nothing, and an unknown live key neither.
    for live in [
        Some(ChartKey {
            generation: 5,
            ..KEY
        }),
        None,
    ] {
        fx.sel.set_chart_live(live);
        fx.frame(20);
        assert!(drawn(&fx, STAGE).is_empty(), "{live:?}");
        assert!(drawn(&fx, BANNER).is_empty(), "{live:?}");
    }
    // An unkeyed table neither.
    fx.sel.set_chart_live(Some(KEY));
    fx.sel
        .set_chart_images(Some(ChartTable::new(Some(STAGE), None, Some(BANNER))));
    fx.frame(30);
    assert!(drawn(&fx, STAGE).is_empty());
    // An absent entry draws nothing; the other still draws.
    fx.sel
        .set_chart_images(Some(ChartTable::new(None, None, Some(BANNER)).keyed(KEY)));
    fx.frame(40);
    assert!(drawn(&fx, STAGE).is_empty());
    assert_eq!(drawn(&fx, BANNER).len(), 1);
}

/// `selected.stagefile` and `selected.banner` read the table set: each
/// image it holds, both false with none set, and a view update resets
/// them until they are written again; conditions read them.
#[test]
fn the_image_fields_read_the_table_set() {
    let mut fx = skin();
    fx.view(&view(vec![chart("a")], Some(0)));
    fx.sel.set_chart_live(Some(KEY));
    fx.sel.write_chart_fields();
    assert_eq!(fields(&fx), [0, 0], "none set");
    fx.frame(0);
    assert!(mark(&fx, 50.0), "190 holds with none set");
    assert!(!mark(&fx, 60.0), "193 does not");

    for (stage, banner) in [(true, false), (false, true), (true, true), (false, false)] {
        fx.sel.set_chart_images(Some(
            ChartTable::new(stage.then_some(STAGE), None, banner.then_some(BANNER)).keyed(KEY),
        ));
        fx.sel.write_chart_fields();
        let want = [i32::from(stage), i32::from(banner)];
        assert_eq!(fields(&fx), want, "{stage} {banner}");
        fx.frame(10);
        assert_eq!(mark(&fx, 50.0), !stage, "190: {stage}");
        assert_eq!(mark(&fx, 60.0), banner, "193: {banner}");
    }

    fx.sel.set_chart_images(Some(
        ChartTable::new(Some(STAGE), None, Some(BANNER)).keyed(KEY),
    ));
    fx.sel.write_chart_fields();
    fx.view(&view(vec![chart("b")], Some(0)));
    assert_eq!(fields(&fx), [0, 0], "a view resets them");
    fx.sel.write_chart_fields();
    assert_eq!(fields(&fx), [1, 1]);
    fx.sel.set_chart_images(None);
    fx.sel.write_chart_fields();
    assert_eq!(fields(&fx), [0, 0], "not current");
}

/// The image fields sit before the settings: a view update keeps the
/// settings and the application's readouts at their own slots, and the
/// fields written after it read the table, leaving both as they were.
#[test]
fn the_image_fields_read_before_the_settings() {
    let mut fx = skin();
    fx.app = skin_model::AppReadouts {
        fps: 57,
        ..skin_model::AppReadouts::LAUNCH
    };
    let settings = skin_model::SelectionSettings {
        display_offset_ms: -7,
        system_volume: skin_model::VolumeReading::of_gain(0.5),
        key_volume: skin_model::VolumeReading::of_gain(0.25),
        background_volume: skin_model::VolumeReading::of_gain(0.75),
        speed: skin_model::SpeedView::Classic,
        order: skin_model::SelectionOrder::Title,
        gauge: None,
        covers_on: [true, false, false],
        play_to_end: false,
        assignment: None,
    };
    fx.view(&view(vec![chart("a")], Some(0)));
    fx.skin
        .update_selection_settings(&mut fx.sel, &settings)
        .unwrap();
    fx.frame(0);
    let kept = |fx: &Fx| {
        let f = fx.sel.state().fields;
        [
            field::SETTINGS_DISPLAY_OFFSET,
            field::SETTINGS_VOLUME_SYSTEM,
            field::SETTINGS_SUDDEN,
            field::SETTINGS_VOLUME_BACKGROUND,
            field::APP_FPS,
        ]
        .map(|s| f[usize::from(s)])
    };
    assert_eq!(kept(&fx), [-7, 50, 1, 75, 57]);
    fx.sel
        .set_chart_images(Some(ChartTable::new(None, None, Some(BANNER)).keyed(KEY)));
    fx.view(&view(vec![chart("b")], Some(0)));
    assert_eq!(kept(&fx), [-7, 50, 1, 75, 57], "a view moved them");
    fx.sel.write_chart_fields();
    assert_eq!(fields(&fx), [0, 1]);
    assert_eq!(
        kept(&fx),
        [-7, 50, 1, 75, 57],
        "the image fields moved them"
    );
}
