//! The chart's metadata on the production path: a chart file loaded as a
//! direct launch loads it (`game::Loaded::load`, its ingestion and the
//! play preset), the game's attempt, this adapter's play context, then the
//! real `Logic`, the render side's acquisition and the production frame
//! adapter (`render::skin_frame`) into an imported skin whose images stand
//! under ops 150 to 155, 170 and 180 to 184 and whose number shows 368
//! (docs/stage3.md R3).

use bms_model::{Lane, TimeUs};
use eval::{Binding, ControlId, DeviceProfile, InputStart, SourceProfile};
use game::{Logic, LogicConfig};
use render::FrameInput;
use skin_beatoraja::{Config, import, read_header};
use skin_model::{AtlasLimits, FrameReadouts, LaneView, QuadBatch, Side, Skin, UvTable, Variant};

use super::AttemptContext;
use crate::imports::facts::tests::{REFERENCE_CLASSES, loaded};

const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 4_096,
    max_layers: 8,
    reserved_layers: 1,
};

/// Each condition's image is drawn `w` wide, 10 high.
const OPS: [(i32, i32); 14] = [
    (150, 10),
    (151, 11),
    (152, 12),
    (153, 13),
    (154, 14),
    (155, 15),
    (170, 20),
    (-170, 21),
    (180, 30),
    (181, 31),
    (182, 32),
    (183, 33),
    (184, 34),
    (-182, 35),
];

/// Writes a `w`×16 white PNG at `path`.
fn png(path: &std::path::Path, w: u32) {
    let file = std::fs::File::create(path).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, 16);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let white: Vec<u8> = (0..w * 16).flat_map(|_| [255; 4]).collect();
    e.write_header().unwrap().write_image_data(&white).unwrap();
}

/// The skin: an image under each of [`OPS`], and number 368 on a ten-glyph
/// strip, each digit 20 high.
pub(crate) fn skin(dir: &std::path::Path) -> (Skin, UvTable) {
    let dsts: Vec<String> = OPS
        .iter()
        .map(|(op, w)| {
            format!(
                r#"{{"id": "i", "op": [{op}], "dst": [{{"x": 0, "y": 0, "w": {w}, "h": 10}}]}}"#
            )
        })
        .collect();
    std::fs::write(
        dir.join("s.json"),
        format!(
            r#"{{"type": 0, "w": 1920, "h": 1080,
                "source": [{{"id": "a", "path": "a.png"}}, {{"id": "n", "path": "n.png"}}],
                "image": [{{"id": "i", "src": "a", "w": 16, "h": 16}}],
                "value": [{{"id": "v", "src": "n", "w": 160, "h": 16, "divx": 10, "digit": 4,
                            "ref": 368}}],
                "destination": [{}, {{"id": "v", "dst": [{{"x": 500, "w": 12, "h": 20}}]}}]}}"#,
            dsts.join(", ")
        ),
    )
    .unwrap();
    png(&dir.join("a.png"), 16);
    png(&dir.join("n.png"), 160);
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

/// The condition images' widths, sorted, and the song total's digit count
/// in the first frame of `loaded`'s attempt, with the total its context
/// gives.
fn drawn(loaded: &game::Loaded, skin: &Skin, uvs: &UvTable) -> (Vec<i32>, usize, Option<i32>) {
    try_drawn(loaded, skin, uvs).unwrap()
}

/// [`drawn`], or the evaluator's refusal of the chart under its preset.
fn try_drawn(
    loaded: &game::Loaded,
    skin: &Skin,
    uvs: &UvTable,
) -> Result<(Vec<i32>, usize, Option<i32>), game::LogicError> {
    let owned = AttemptContext::new(loaded.attempt(), Side::Left, 0, false);
    let ctx = owned.context();
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
    )?;
    let mut play = skin.prepare(loaded.chart(), &ctx).unwrap();
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let view = side.acquire(16, |_| {}).unwrap();
    let f = FrameInput {
        app: skin_model::AppReadouts::LAUNCH,
        state_us: 0,
        now: TimeUs(-500_000),
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
    skin.build(&mut play, &render::skin_frame(&f), uvs, &mut batch)
        .unwrap();
    let mut widths = Vec::new();
    let mut digits = 0;
    for q in batch.quads() {
        #[expect(clippy::cast_possible_truncation, reason = "test rects are small")]
        let [w, h] = [q.rect[2].round() as i32, q.rect[3].round() as i32];
        if h == 10 {
            widths.push(w);
        } else {
            digits += 1;
        }
    }
    widths.sort_unstable();
    Ok((widths, digits, ctx.meta.total))
}

/// Charts with and without each header, loaded as a direct launch loads
/// them, under the play presets: the difficulty selects 150 to 155 (0 for
/// none or one outside 1 to 5), a `#BMPxx` clears 170, the class of the
/// chart's own rank selects 180 to 184 (`#RANK 2`'s without one, 182, so
/// -182 stays clear), and 368 shows the `#TOTAL` the reference reads, else
/// its default (260 below 338 notes): three digits each.
#[test]
fn chart_metadata_reaches_the_frames_from_a_loaded_chart() {
    let dir = tempfile::tempdir().unwrap();
    let (skin, uvs) = skin(dir.path());
    for (text, preset, widths, total) in [
        (
            "#DIFFICULTY 4\n#RANK 0\n#TOTAL 300.7\n#BMP01 a.bmp\n",
            "beatoraja-style",
            vec![14, 21, 30, 35],
            300,
        ),
        ("", "beatoraja-style", vec![10, 20, 32], 260),
        ("", "tapestry", vec![10, 20, 32], 260),
        (
            "#DIFFICULTY 9\n#DEFEXRANK 120\n#TOTAL 0\n",
            "beatoraja-style",
            vec![10, 20, 33, 35],
            260,
        ),
        (
            "#DIFFICULTY 1\n#RANK 4\n#TOTAL 999.9\n",
            "beatoraja-style",
            vec![11, 20, 34, 35],
            999,
        ),
        // The tapestry preset judges `#RANK 7` as written, in no band; the
        // class is the chart's own, `#RANK 2`'s.
        (
            "#DIFFICULTY 5\n#RANK 7\n",
            "tapestry",
            vec![15, 20, 32],
            260,
        ),
        // `#DEFEXRANK 10` is 7 there, in no class: none holds, -182 does.
        // The tapestry preset's own default total does not move 368.
        (
            "#DIFFICULTY 2\n#DEFEXRANK 10\n",
            "tapestry",
            vec![12, 20, 35],
            260,
        ),
    ] {
        let loaded = loaded(text, preset);
        assert_eq!(
            drawn(&loaded, &skin, &uvs),
            (widths, 3, Some(total)),
            "{text:?} {preset}"
        );
    }
}

/// Every chart of [`REFERENCE_CLASSES`], under either play preset, draws
/// the image of its class's op alone among 180 to 184 (none for a chart in
/// no class), and -182 unless the class is normal. The evaluator refuses
/// to judge some of them: under the tapestry preset a rank it reads as
/// written (`#RANK 100`), under either one a `#DEFEXRANK` whose windows
/// leave the chart time domain, from `#DEFEXRANK 1974698`
/// (`rules::beatoraja_style`'s known differences).
#[test]
fn the_class_of_each_reference_chart_reaches_the_frames() {
    let past_the_domain = |text: &str| {
        text.strip_prefix("#DEFEXRANK ")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .is_some_and(|n| n >= 1_974_698)
    };
    let dir = tempfile::tempdir().unwrap();
    let (skin, uvs) = skin(dir.path());
    for &(text, _, class) in REFERENCE_CLASSES {
        for preset in ["tapestry", "beatoraja-style"] {
            let (widths, ..) = match try_drawn(&loaded(text, preset), &skin, &uvs) {
                Ok(d) => {
                    assert!(!past_the_domain(text), "{text:?} {preset}");
                    d
                }
                Err(game::LogicError::Eval(_)) if preset == "tapestry" || past_the_domain(text) => {
                    continue;
                }
                Err(e) => panic!("{text:?} {preset}: {e:?}"),
            };
            let classes: Vec<i32> = widths.into_iter().filter(|&w| w >= 30).collect();
            let mut want: Vec<i32> = class
                .map(|c| 30 + i32::from(c.index()))
                .into_iter()
                .collect();
            if class != Some(skin_beatoraja::JudgeClass::Normal) {
                want.push(35);
            }
            assert_eq!(classes, want, "{text:?} {preset}");
        }
    }
}
