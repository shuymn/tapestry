//! The recent timing on the production path, deterministically (docs/
//! stage3.md R3): input through real `Logic`, the render side's
//! acquisition and the production frame adapter (`render::skin_frame`)
//! into an imported skin, each frame at a chosen time and acknowledged
//! before the next advance, so no frame draws an older view than the step
//! it checks. Number 525's digits and the images on ops 1242 (early) and
//! 1243 (late), through a rearm and a retry.

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use eval::{
    Binding, ControlId, DeviceProfile, InputKind, InputStart, JudgeEvent, PhysicalInputEvent,
    SourceId, SourceProfile,
};
use game::{Logic, LogicConfig, RenderSide};
use input::{SourceReader, SourceWriter, source_queue};
use render::FrameInput;
use skin_beatoraja::{Config, import, read_header};
use skin_model::{
    AtlasLimits, Attempt, FrameReadouts, GaugeDesc, LaneView, PlayContext, Quad, QuadBatch, Side,
    Skin, UvTable, Variant,
};

const MS: i64 = 1_000;

const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 4_096,
    max_layers: 8,
    reserved_layers: 1,
};

const CONFIG: LogicConfig = LogicConfig {
    inputs: 256,
    effects: 256,
};

fn lane(n: u8) -> Lane {
    Lane::new(n).unwrap()
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

/// Key 1's taps at 1, 2, 3 and 5.4 s, and key 2's long note from 6 to
/// 6.5 s.
fn chart() -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(2_000 * MS))
        .tap(lane(1), TimeUs(3_000 * MS))
        .tap(lane(1), TimeUs(5_400 * MS))
        .long(lane(2), TimeUs(6_000 * MS), TimeUs(6_500 * MS))
        .build()
        .unwrap()
}

/// The skin: number 525 on a 24-glyph strip (cells 12 wide), an image 30
/// wide on op 1242 and one 40 wide on op 1243.
fn skin(dir: &std::path::Path) -> (Skin, UvTable) {
    std::fs::write(
        dir.join("s.json"),
        r#"{"type": 0, "w": 1920, "h": 1080,
            "source": [{"id": "a", "path": "a.png"}, {"id": "n", "path": "n.png"}],
            "value": [{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 24, "digit": 4,
                       "ref": 525}],
            "image": [{"id": "i", "src": "a", "w": 16, "h": 16}],
            "destination": [{"id": "v", "dst": [{"w": 12, "h": 20}]},
                            {"id": "i", "op": [1242], "dst": [{"y": 100, "w": 30, "h": 10}]},
                            {"id": "i", "op": [1243], "dst": [{"y": 100, "w": 40, "h": 10}]}]}"#,
    )
    .unwrap();
    for (name, w) in [("a.png", 16), ("n.png", 120)] {
        let file = std::fs::File::create(dir.join(name)).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, 20);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let white: Vec<u8> = (0..w * 20).flat_map(|_| [255; 4]).collect();
        e.write_header().unwrap().write_image_data(&white).unwrap();
    }
    let (mut def, h) = read_header(dir, "s.json").unwrap();
    let imported = import(
        &mut def,
        &h,
        &Config::defaults(&h),
        skin_model::ScreenKind::Play,
    )
    .unwrap();
    assert!(
        imported.report.notes.is_empty(),
        "{:?}",
        imported.report.notes
    );
    let skin = imported.skin;
    let uvs = skin
        .atlas(Variant::Lo, LIMITS, &|| false)
        .unwrap()
        .into_parts()
        .1;
    (skin, uvs)
}

/// What a frame shows: number 525, `None` when undrawn, and whether the
/// early and late images drew.
#[derive(Debug, PartialEq, Eq)]
struct Shown {
    ms: Option<i32>,
    early: bool,
    late: bool,
}

const fn shown(ms: i32, early: bool, late: bool) -> Shown {
    Shown {
        ms: Some(ms),
        early,
        late,
    }
}

/// One attempt's logic, its render side and the skin's state for it.
struct Rig<'a> {
    logic: Logic<'a, SourceReader>,
    side: RenderSide,
    writer: SourceWriter,
    play: skin_model::Play,
    batch: QuadBatch,
    effects: Vec<JudgeEvent>,
    /// The view generation the skin drew last.
    generation: u64,
    /// The uv index of the strip's first-half 0.
    zero: Option<usize>,
}

impl<'a> Rig<'a> {
    fn new(
        chart: &'a Chart,
        rule: &'a rules::RulePreset,
        dev: &'a DeviceProfile,
        skin: &Skin,
        ctx: &PlayContext<'_>,
    ) -> Self {
        let (writer, reader) = source_queue(64);
        let (logic, side) = Logic::new(
            chart,
            rule,
            dev,
            &InputStart::default(),
            vec![reader],
            CONFIG,
            Vec::new(),
        )
        .unwrap();
        Self {
            logic,
            side,
            writer,
            play: skin.prepare(chart, ctx).unwrap(),
            batch: QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity()),
            effects: Vec::with_capacity(CONFIG.effects),
            generation: 0,
            zero: None,
        }
    }

    /// Lane `n` set to `value` at `ms`.
    fn set(&mut self, us: i64, n: u8, value: i32) {
        self.writer
            .push(PhysicalInputEvent {
                received: TimeUs(us),
                source: SourceId(0),
                kind: InputKind::Control {
                    control: ControlId(u32::from(n)),
                    value,
                },
            })
            .unwrap();
    }

    /// Publishes the source up to `ms`, advances the logic there, and
    /// draws the frame at `ms`: every effect the step emitted is acquired
    /// in this one frame, none split or dropped.
    fn step(&mut self, skin: &Skin, uvs: &UvTable, ms: i64) -> Shown {
        let now = TimeUs(ms * MS);
        self.writer.publish(now);
        let emitted = self.logic.advance_to(now).emitted;
        self.frame(skin, uvs, now, emitted)
    }

    /// The frame at `now` from the render side's acquisition, rearming the
    /// skin when the view is a new candidate's, as the render thread does;
    /// `emitted` effects are expected.
    fn frame(&mut self, skin: &Skin, uvs: &UvTable, now: TimeUs, emitted: usize) -> Shown {
        self.effects.clear();
        let effects = &mut self.effects;
        let room = effects.capacity();
        let view = self.side.acquire(room, |e| effects.push(*e)).unwrap();
        assert_eq!(self.effects.len(), emitted, "an effect split or dropped");
        if view.generation != self.generation {
            self.generation = view.generation;
            self.play.rearm();
        }
        let f = FrameInput {
            now,
            display_offset: TimeUs(0),
            lanes: LaneView::NONE,
            readouts: FrameReadouts::UNAVAILABLE,
            view,
            effects: &self.effects,
            gauge: 0,
            overlay: &[],
            target: [1_920, 1_080],
            turntable: 0,
        };
        self.batch.clear();
        skin.build(
            &mut self.play,
            &render::skin_frame(&f),
            uvs,
            &mut self.batch,
        )
        .unwrap();
        let quads = self.batch.quads().to_vec();
        self.read(uvs, &quads)
    }

    /// Reads `quads`: the number's cells are 12 wide, the early image 30
    /// and the late 40. The first frame read, before any judgment, names
    /// the strip from its 0.
    fn read(&mut self, uvs: &UvTable, quads: &[Quad]) -> Shown {
        let width = |q: &Quad| q.rect[2].round() as i32;
        let index = |q: &Quad| {
            uvs.frames()
                .iter()
                .position(|f| f.uv == q.uv && f.layer == q.layer)
                .unwrap()
        };
        let mut cells: Vec<&Quad> = quads.iter().filter(|q| width(q) == 12).collect();
        cells.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
        let zero = *self.zero.get_or_insert_with(|| {
            assert_eq!(cells.len(), 2, "the first frame reads 0");
            index(cells[1])
        });
        let ms = (!cells.is_empty()).then(|| {
            let glyphs: Vec<usize> = cells.iter().map(|q| index(q) - zero).collect();
            let (sign, digits) = glyphs.split_first().unwrap();
            assert_eq!(sign % 12, 11, "{glyphs:?}");
            let magnitude = digits.iter().fold(0, |v, &g| {
                assert!(g % 12 < 10 && g / 12 == sign / 12, "{glyphs:?}");
                v * 10 + i32::try_from(g % 12).unwrap()
            });
            if *sign >= 12 { -magnitude } else { magnitude }
        });
        Shown {
            ms,
            early: quads.iter().any(|q| width(q) == 30),
            late: quads.iter().any(|q| width(q) == 40),
        }
    }
}

/// Under the beatoraja-style preset: 30 ms early, GREAT (+30, early); a
/// rearm (0, neither) whose candidate misses the first note (−210, late);
/// the second on time, PERFECT (0); a press 0.5 ms after it, an empty POOR
/// truncated to 0 (neither); the third missed (−210); a press 300 ms before
/// the fourth, an empty POOR (+300, early); the fourth missed (−210); the
/// long note's head 10 ms early, held, which leaves −210; its release 40 ms
/// before its end, PERFECT, farther than the head (+40, neither). A retry
/// starts at 0 and takes its own first judgment.
#[test]
fn the_recent_timing_reaches_the_skin_through_logic_and_the_frame() {
    let dir = tempfile::tempdir().unwrap();
    let (skin, uvs) = skin(dir.path());
    let chart = chart();
    let rule = rules::beatoraja_style();
    let dev = device();
    let gauges: Vec<GaugeDesc> = rule.gauges.iter().map(GaugeDesc::from_rule).collect();
    let ctx = PlayContext {
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: &gauges,
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: rule.empty_poor.breaks_combo,
        empty_poor_counted: rule.empty_poor.counted,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(i64::MAX),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
    };
    let mut r = Rig::new(&chart, &rule, &dev, &skin, &ctx);
    assert_eq!(
        r.step(&skin, &uvs, 500),
        shown(0, false, false),
        "before any"
    );
    r.set(970 * MS, 1, 1);
    r.set(975 * MS, 1, 0);
    assert_eq!(
        r.step(&skin, &uvs, 980),
        shown(30, true, false),
        "early GREAT"
    );
    // A rearmed candidate: its view is the next generation, and the frame
    // starts the skin over.
    r.logic.rearm(&InputStart::default(), TimeUs(0));
    let rearmed = r.frame(&skin, &uvs, TimeUs(990 * MS), 0);
    assert_eq!(r.generation, 1);
    assert_eq!(rearmed, shown(0, false, false), "rearmed");
    assert_eq!(r.step(&skin, &uvs, 1_300), shown(-210, false, true), "miss");
    r.set(2_000 * MS, 1, 1);
    r.set(2_000 * MS + 300, 1, 0);
    assert_eq!(
        r.step(&skin, &uvs, 2_000),
        shown(0, false, false),
        "PERFECT"
    );
    r.set(2_000 * MS + 500, 1, 1);
    r.set(2_020 * MS, 1, 0);
    assert_eq!(r.step(&skin, &uvs, 2_001), shown(0, false, false), "sub-ms");
    assert_eq!(
        r.step(&skin, &uvs, 3_300),
        shown(-210, false, true),
        "miss 3"
    );
    r.set(5_100 * MS, 1, 1);
    r.set(5_120 * MS, 1, 0);
    assert_eq!(
        r.step(&skin, &uvs, 5_150),
        shown(300, true, false),
        "empty POOR"
    );
    assert_eq!(
        r.step(&skin, &uvs, 5_700),
        shown(-210, false, true),
        "miss 4"
    );
    r.set(5_990 * MS, 2, 1);
    assert_eq!(
        r.step(&skin, &uvs, 6_000),
        shown(-210, false, true),
        "held head"
    );
    r.set(6_460 * MS, 2, 0);
    assert_eq!(
        r.step(&skin, &uvs, 6_470),
        shown(40, false, false),
        "release"
    );
    // A retry: a fresh attempt and preparation.
    let mut r = Rig::new(&chart, &rule, &dev, &skin, &ctx);
    assert_eq!(r.step(&skin, &uvs, 500), shown(0, false, false), "retried");
    r.set(970 * MS, 1, 1);
    r.set(975 * MS, 1, 0);
    assert_eq!(
        r.step(&skin, &uvs, 980),
        shown(30, true, false),
        "after the retry"
    );
}
