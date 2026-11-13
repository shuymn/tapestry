//! The chart's own images on a result skin (S10-12): a destination of
//! system image 100 or 101 becomes a chart image, as on play, and options
//! 190, 191, 194 and 195 read the attempt's resolved availability
//! (`chart.stagefile`, `chart.backbmp`) with no guard, in a frame's
//! conditions and in a Lua callback alike. The trimmed stretches stay
//! dropped. Shapes of real result skins are kept: an image under the
//! clear, a custom option and 191, and an extend frame under 191 and a
//! custom option.

mod common;

use common::result::{Res, folder, graph_json, import_result, ready};
use common::{kinds, play::round};
use skin_beatoraja::NoteKind;
use skin_model::chart_image::{ChartKey, ChartStretch, ChartTable, ChartUv};
use skin_model::{FrameUv, ImageFacts, ResultView};

const KEY: ChartKey = ChartKey {
    renderer: 1,
    installation: 2,
    atlas: 3,
    generation: 4,
};

/// A 1 600×1 200 stage file and a 512×384 back image in layer 9.
const STAGE: ChartUv = ChartUv {
    uv: FrameUv {
        uv: [0.0, 0.0, 0.390_625, 0.292_968_75],
        layer: 9,
    },
    native: [1_600, 1_200],
};
const BACK: ChartUv = ChartUv {
    uv: FrameUv {
        uv: [0.5, 0.0, 0.625, 0.093_75],
        layer: 9,
    },
    native: [512, 384],
};

fn table(stage: bool, back: bool) -> ChartTable {
    ChartTable::new(stage.then_some(STAGE), back.then_some(BACK), None).keyed(KEY)
}

/// The model rect (y down, 1080 high) of a reference rect (y up).
fn model([x, y, w, h]: [i32; 4]) -> [f32; 4] {
    [x as f32, (1_080 - y - h) as f32, w as f32, h as f32]
}

/// A ready view whose chart has `images`.
fn view(images: ImageFacts) -> ResultView {
    let mut v = ready(
        1_234,
        true,
        common::result::best(skin_model::ResultNumber::Shown(3)),
    );
    v.chart.images = images;
    v
}

/// A mark of the fixture's dot under `op` at x `x`, y 500.
fn mark(op: &str, x: i32) -> String {
    format!(r#"{{"id": "dot", "op": [{op}], "dst": [{{"x": {x}, "y": 500, "w": 8, "h": 8}}]}}"#)
}

/// −100 stretch 1 and −101 under 195 lower as chart images, a trimmed
/// stretch (3) is dropped with its note, and 190, 191, 194 and 195 (and a
/// negated 191) read the view's availability, not the table: with the
/// table holding both images, a chart whose back image is absent draws
/// the stage file and not the back image, and marks 191 and 194.
#[test]
fn result_chart_images_follow_the_attempts_availability() {
    let dsts = format!(
        r#"{{"id": "-100", "stretch": 1, "dst": [{{"x": 100, "y": 200, "w": 300, "h": 120}}]}},
           {{"id": "-100", "stretch": 3, "dst": [{{"x": 0, "y": 0, "w": 10, "h": 10}}]}},
           {{"id": "-101", "op": [195], "dst": [{{"x": 500, "y": 200, "w": 64, "h": 48}}]}},
           {}, {}, {}, {}, {}"#,
        mark("190", 1_000),
        mark("191", 1_010),
        mark("194", 1_020),
        mark("195", 1_030),
        mark("-191", 1_040),
    );
    let d = folder(&graph_json("", &dsts));
    let i = import_result(d.path()).unwrap();
    assert_eq!(
        kinds(&i.report)
            .into_iter()
            .filter(|k| matches!(k, NoteKind::Stretch(_)))
            .collect::<Vec<_>>(),
        [NoteKind::Stretch(3)]
    );
    assert!(i.skin.has_chart_images());
    let mut r = Res::new(i.skin);
    r.res.set_chart_live(Some(KEY));
    r.res.set_chart_images(table(true, true));
    let marks = |r: &mut Res| -> Vec<i32> {
        let mut x: Vec<i32> = r
            .frame(0)
            .iter()
            .map(|q| round(q.rect))
            .filter(|q| q[1] == 1_080 - 500 - 8 && q[0] >= 1_000)
            .map(|q| q[0])
            .collect();
        x.sort_unstable();
        x
    };
    let charts = |r: &mut Res| -> Vec<[f32; 4]> {
        r.frame(0)
            .iter()
            .filter(|q| q.uv == STAGE.uv.uv || q.uv == BACK.uv.uv)
            .map(|q| q.rect)
            .collect()
    };
    r.view(&view(ImageFacts {
        stage: true,
        back: false,
    }));
    assert_eq!(marks(&mut r), [1_010, 1_020]);
    let want = ChartStretch::Inner
        .place(model([100, 200, 300, 120]), STAGE.native)
        .0;
    assert_eq!(
        charts(&mut r).iter().map(|q| round(*q)).collect::<Vec<_>>(),
        [round(want)]
    );
    r.view(&view(ImageFacts {
        stage: false,
        back: true,
    }));
    assert_eq!(marks(&mut r), [1_000, 1_030, 1_040]);
    assert_eq!(
        charts(&mut r).len(),
        2,
        "the stage file draws from the table"
    );
    // An absent table draws neither, whatever the facts say.
    r.res.set_chart_images(ChartTable::ABSENT);
    assert!(charts(&mut r).is_empty());
}

/// The shapes of real result skins are kept: the stage file under the
/// clear (90), a custom option and 191, and an extend frame under 191 and
/// another custom option; each draws once its options hold.
#[test]
fn real_result_shapes_keep_their_chart_images() {
    let body = format!(
        r#""property": [{{"name": "main", "item": [{{"name": "on", "op": 932}}, {{"name": "off", "op": 933}}], "def": "on"}},
                        {{"name": "frame", "item": [{{"name": "on", "op": 917}}, {{"name": "off", "op": 918}}], "def": "on"}}],
           {}"#,
        graph_json(
            "",
            r#"{"id": "-100", "op": [90, 932, 191], "stretch": 1, "dst": [{"x": 0, "y": 0, "w": 1920, "h": 1080}]},
               {"id": "dot", "op": [191, 917], "dst": [{"x": 700, "y": 500, "w": 8, "h": 8}]}"#,
        )
    );
    let d = folder(&body);
    let i = import_result(d.path()).unwrap();
    assert!(
        kinds(&i.report).is_empty(),
        "nothing dropped: {:?}",
        kinds(&i.report)
    );
    let mut r = Res::new(i.skin);
    r.res.set_chart_live(Some(KEY));
    r.res.set_chart_images(table(true, false));
    r.view(&view(ImageFacts {
        stage: true,
        back: false,
    }));
    let quads = r.frame(0);
    assert!(quads.iter().any(|q| q.uv == STAGE.uv.uv), "the stage file");
    assert!(
        quads.iter().any(|q| round(q.rect)[0] == 700),
        "the extend frame"
    );
    r.view(&view(ImageFacts::default()));
    let quads = r.frame(0);
    assert!(!quads.iter().any(|q| q.uv == STAGE.uv.uv));
    assert!(!quads.iter().any(|q| round(q.rect)[0] == 700));
}
