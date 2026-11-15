//! The play readouts' end on the production path: a chart file loaded as a
//! direct launch loads it (`game::Loaded::load`, its parser and the play
//! preset), played or autoplayed, the game's attempt, this adapter's play
//! context, then the real `Logic`, the render side's acquisition and the
//! production frame adapter (`render::skin_frame`) into an imported skin
//! with a type-6 slider and graph 101, which declares a `finishmargin`
//! (design.md "Play Lifecycle", Audio tail; "Rendering and Skins", play
//! readouts).

use bms_model::{Lane, TimeUs};
use eval::{Binding, ControlId, DeviceProfile, InputStart, SourceProfile};
use game::{Logic, LogicConfig};
use render::FrameInput;
use skin_beatoraja::{Config, import, read_header};
use skin_model::{
    AtlasLimits, FrameReadouts, LaneView, QuadBatch, Side, Skin, UvTable, Variant, field,
};

use super::AttemptContext;
use crate::imports::facts::tests::loaded_session;

const MS: i64 = 1_000;

const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 4_096,
    max_layers: 8,
    reserved_layers: 1,
};

/// The skin's declared finish margin.
const FINISH_MARGIN_MS: i64 = 1_500;

/// The chart, after the helper's tap at 2 s: a tap at 17 s and a
/// background sound at 19 s, its last object. A normal play finishes at
/// 22 s, an autoplay at 24 s.
const CHART: &str = "#WAV01 a.wav\n#00811:0001\n#00901:0001\n";

/// The slider's x at rest, its range to the right, and graph 101's full
/// width, in reference pixels on a 1 920 × 1 080 target.
const SLIDER_X: f32 = 100.0;
const SLIDER_RANGE: f32 = 1_000.0;
const GRAPH_W: f32 = 200.0;

/// The skin: a type-6 slider 10 × 10 at x 100 moving right by 1 000, graph
/// 101 across 200 × 20 at y 340, and a top-level `finishmargin`.
fn skin(dir: &std::path::Path) -> (Skin, UvTable) {
    std::fs::write(
        dir.join("s.json"),
        format!(
            r#"{{"type": 0, "w": 1920, "h": 1080, "finishmargin": {FINISH_MARGIN_MS},
                "source": [{{"id": "a", "path": "a.png"}}],
                "slider": [{{"id": "p", "src": "a", "w": 16, "h": 16, "type": 6, "angle": 1,
                            "range": 1000}}],
                "graph": [{{"id": "g", "src": "a", "w": 16, "h": 16, "type": 101,
                           "angle": 0}}],
                "destination": [
                    {{"id": "p", "dst": [{{"x": 100, "y": 500, "w": 10, "h": 10}}]}},
                    {{"id": "g", "dst": [{{"x": 100, "y": 340, "w": 200, "h": 20}}]}}]}}"#
        ),
    )
    .unwrap();
    let file = std::fs::File::create(dir.join("a.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 16, 16);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let white: Vec<u8> = (0..16 * 16).flat_map(|_| [255; 4]).collect();
    e.write_header().unwrap().write_image_data(&white).unwrap();
    let (mut def, h) = read_header(dir, "s.json").unwrap();
    let skin = import(
        &mut def,
        &h,
        &Config::defaults(&h),
        skin_model::ScreenKind::Play,
    )
    .unwrap()
    .skin;
    let uvs = skin
        .atlas(Variant::Lo, LIMITS, &|| false)
        .unwrap()
        .into_parts()
        .1;
    (skin, uvs)
}

/// One source binding control `n` to lane `n`.
fn device() -> DeviceProfile {
    DeviceProfile {
        sources: vec![SourceProfile {
            input_offset: TimeUs(0),
            bindings: Lane::all()
                .map(|l| Binding {
                    control: ControlId(u32::try_from(l.index()).unwrap()),
                    lane: l,
                })
                .collect(),
            axes: Vec::new(),
        }],
    }
}

/// What a frame shows: the remaining seconds, the progress in ppm, the
/// slider's x and graph 101's width.
#[derive(Debug)]
struct Shown {
    remaining: i32,
    ppm: i32,
    slider_x: f32,
    graph_w: f32,
}

/// The frames of `loaded`'s attempt at each of `at_ms`, played or
/// autoplayed as `autoplay`, with the finish its context carries.
fn frames(
    loaded: &game::Loaded,
    autoplay: bool,
    skin: &Skin,
    uvs: &UvTable,
    at_ms: &[i64],
) -> (TimeUs, Vec<Shown>) {
    let owned = AttemptContext::new(loaded.attempt(), Side::Left, 0, autoplay);
    let ctx = owned.context();
    let skin_model::Attempt::Full { finish, .. } = ctx.attempt else {
        panic!("a whole-chart attempt: {:?}", ctx.attempt);
    };
    let dev = device();
    let (_writer, reader) = input::source_queue(8);
    let (_logic, mut side) = Logic::with_section(
        loaded.chart(),
        loaded.preset(),
        &dev,
        &InputStart::default(),
        None,
        vec![reader],
        LogicConfig {
            inputs: 16,
            effects: 16,
        },
        Vec::new(),
    )
    .unwrap();
    let mut play = skin.prepare(loaded.chart(), &ctx).unwrap();
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let mut shown = Vec::new();
    for &ms in at_ms {
        let view = side.acquire(16, |_| {}).unwrap();
        let f = FrameInput {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            screen_us: 0,
            now: TimeUs(ms * MS),
            display_offset: TimeUs(0),
            lanes: LaneView::NONE,
            readouts: FrameReadouts::UNAVAILABLE,
            view,
            effects: &[],
            gauge: 0,
            overlay: &[],
            target: [1_920, 1_080],
            turntable: 0,
        };
        batch.clear();
        skin.build(&mut play, &render::skin_frame(&f), uvs, &mut batch)
            .unwrap();
        let slider = batch.quads().iter().find(|q| q.rect[3] == 10.0);
        let graph = batch.quads().iter().find(|q| q.rect[3] == 20.0);
        let fields = play.state().fields;
        shown.push(Shown {
            remaining: fields[usize::from(field::TIME_REMAINING_S)],
            ppm: fields[usize::from(field::TIME_PROGRESS_PPM)],
            slider_x: slider.map_or(f32::NAN, |q| q.rect[0]),
            graph_w: graph.map_or(0.0, |q| q.rect[2]),
        });
    }
    (finish, shown)
}

/// A played chart's readouts end at its finish point, the last note at
/// 17 s plus 5 s, and an autoplayed one's at its last object, the
/// background sound at 19 s, plus 5 s; READY holds the attempt's length;
/// the slider and graph 101 follow the progress; and the skin's
/// `finishmargin` is not counted: at 22 s the readouts are done while the
/// hold, which starts from the same cached finish, runs to 23.5 s.
#[test]
fn the_readouts_end_at_the_finish_point_of_a_loaded_chart() {
    let dir = tempfile::tempdir().unwrap();
    let (skin, uvs) = skin(dir.path());
    // The import read the declaration (the positive control for the hold).
    let ending = skin.play_ending();
    assert_eq!(ending.finish_margin, TimeUs(FINISH_MARGIN_MS * MS));
    for (autoplay, finish_s, rows) in [
        (
            false,
            22,
            &[
                (-500, 22, 0),
                (0, 22, 0),
                (10_000, 12, 454_545),
                (17_000, 5, 772_727),
                (20_000, 2, 909_090),
                (22_000, 0, 1_000_000),
                (24_000, 0, 1_000_000),
            ][..],
        ),
        (
            true,
            24,
            &[
                (-500, 24, 0),
                (12_000, 12, 500_000),
                (20_000, 4, 833_333),
                (22_000, 2, 916_666),
                (24_000, 0, 1_000_000),
            ][..],
        ),
    ] {
        let (_chart_dir, loaded) = loaded_session(CHART, "tapestry", autoplay);
        let at: Vec<i64> = rows.iter().map(|r| r.0).collect();
        let (finish, shown) = frames(&loaded, autoplay, &skin, &uvs, &at);
        let finish_us = TimeUs(finish_s * 1_000 * MS);
        assert_eq!(finish, finish_us, "autoplay {autoplay}");
        assert_eq!(finish, loaded.finish(), "autoplay {autoplay}");
        let hold = game::hold_until(
            loaded.finish(),
            game::PlayEnding {
                finish_margin: ending.finish_margin,
                fadeout: ending.fadeout,
            },
        );
        assert_eq!(
            hold,
            TimeUs(finish_us.0 + FINISH_MARGIN_MS * MS),
            "autoplay {autoplay}"
        );
        for (&(ms, remaining, ppm), s) in rows.iter().zip(&shown) {
            let what = format!("autoplay {autoplay}, {ms} ms: {s:?}");
            assert_eq!((s.remaining, s.ppm), (remaining, ppm), "{what}");
            #[expect(clippy::cast_precision_loss, reason = "ppm below 2^24")]
            let p = ppm as f32 / 1_000_000.0;
            assert!(
                (s.slider_x - (SLIDER_X + SLIDER_RANGE * p)).abs() < 0.01,
                "{what}"
            );
            assert!((s.graph_w - GRAPH_W * p).abs() < 0.01, "{what}");
        }
    }
}
