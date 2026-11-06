//! The lanes' visual states on the production path, deterministically
//! (docs/stage3.md R3): input through real `Logic`, the render side's
//! acquisition and the production frame adapter (`render::skin_frame`)
//! into an imported skin, each frame at a chosen time and acknowledged
//! before the next advance, so no frame draws an older view than the step
//! it checks. Image groups on image-index properties 500 (the scratch) to
//! 507 (key 7), through a rearm and a retry.

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use eval::{
    AxisBinding, AxisMode, Binding, ControlId, DeviceProfile, InputKind, InputStart, JudgeEvent,
    PhysicalInputEvent, ScratchSettings, SourceId, SourceProfile,
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

/// The scratch's axis, 256 positions a revolution.
const AXIS: u8 = 8;

/// One source binding control `n` to lane `n`, and the axis to the
/// scratch.
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
            axes: vec![AxisBinding {
                control: ControlId(u32::from(AXIS)),
                lane: Lane::SCRATCH,
                scratch: ScratchSettings {
                    mode: AxisMode::Absolute { wrap: 256 },
                    ..ScratchSettings::DEFAULT
                },
            }],
        }],
    }
}

/// The scratch's tap at 1 s and long note from 6.5 to 7 s; key 1's taps
/// at 1, 2 and 3 s; key 2's long note from 4 to 4.5 s; key 3's taps at 5
/// and 5.5 s; key 4's taps at 6 and 8.5 s; key 5's long note from 7.5
/// to 8 s.
fn chart() -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(0), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(2_000 * MS))
        .tap(lane(1), TimeUs(3_000 * MS))
        .long(lane(2), TimeUs(4_000 * MS), TimeUs(4_500 * MS))
        .tap(lane(3), TimeUs(5_000 * MS))
        .tap(lane(3), TimeUs(5_500 * MS))
        .tap(lane(4), TimeUs(6_000 * MS))
        .long(lane(0), TimeUs(6_500 * MS), TimeUs(7_000 * MS))
        .long(lane(5), TimeUs(7_500 * MS), TimeUs(8_000 * MS))
        .tap(lane(4), TimeUs(8_500 * MS))
        .tap(lane(6), TimeUs(9_000 * MS))
        .long(lane(7), TimeUs(9_500 * MS), TimeUs(10_000 * MS))
        .long(lane(7), TimeUs(10_500 * MS), TimeUs(11_000 * MS))
        .tap(lane(6), TimeUs(12_000 * MS))
        .tap(lane(6), TimeUs(12_000 * MS))
        .build()
        .unwrap()
}

/// The skin: eight image groups of twelve 10-wide frames, `ref` 500 to
/// 507, lane `n`'s at `x = 100 · n`.
fn skin(dir: &std::path::Path) -> (Skin, UvTable) {
    let images: Vec<String> = (0..8)
        .map(|n| {
            format!(
                r#"{{"id": "g{n}", "src": "n", "w": 120, "h": 20, "divx": 12, "len": 12,
                     "ref": {}}}"#,
                500 + n
            )
        })
        .collect();
    let dsts: Vec<String> = (0..8)
        .map(|n| {
            format!(
                r#"{{"id": "g{n}", "dst": [{{"x": {}, "w": 10, "h": 10}}]}}"#,
                100 * n
            )
        })
        .collect();
    std::fs::write(
        dir.join("s.json"),
        format!(
            r#"{{"type": 0, "w": 1920, "h": 1080,
                "source": [{{"id": "n", "path": "n.png"}}],
                "image": [{}], "destination": [{}]}}"#,
            images.join(","),
            dsts.join(",")
        ),
    )
    .unwrap();
    let file = std::fs::File::create(dir.join("n.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 120, 20);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let white: Vec<u8> = (0..120 * 20).flat_map(|_| [255; 4]).collect();
    e.write_header().unwrap().write_image_data(&white).unwrap();
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
    /// Per lane, the uv index of its group's first frame.
    zero: Option<[usize; 8]>,
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

    /// Control `n` set to `value` at `ms`.
    fn set(&mut self, ms: i64, n: u8, value: i32) {
        self.writer
            .push(PhysicalInputEvent {
                received: TimeUs(ms * MS),
                source: SourceId(0),
                kind: InputKind::Control {
                    control: ControlId(u32::from(n)),
                    value,
                },
            })
            .unwrap();
    }

    /// Control `n` pressed at `ms` and released 5 ms later.
    fn tap(&mut self, ms: i64, n: u8) {
        self.set(ms, n, 1);
        self.set(ms + 5, n, 0);
    }

    /// Publishes the source up to `ms`, advances the logic there, and
    /// draws the frame at `ms`: every effect the step emitted is acquired
    /// in this one frame, none split or dropped.
    fn step(&mut self, skin: &Skin, uvs: &UvTable, ms: i64) -> [usize; 8] {
        let now = TimeUs(ms * MS);
        self.writer.publish(now);
        let emitted = self.logic.advance_to(now).emitted;
        self.frame(skin, uvs, now, emitted)
    }

    /// The frame at `now` from the render side's acquisition, rearming the
    /// skin when the view is a new candidate's, as the render thread does;
    /// `emitted` effects are expected.
    fn frame(&mut self, skin: &Skin, uvs: &UvTable, now: TimeUs, emitted: usize) -> [usize; 8] {
        let shown = self.bounded(skin, uvs, now, CONFIG.effects);
        assert_eq!(self.effects.len(), emitted, "an effect split or dropped");
        shown
    }

    /// The frame at `now`, acquiring at most `room` effects; the rest
    /// stay queued for the next frame.
    fn bounded(&mut self, skin: &Skin, uvs: &UvTable, now: TimeUs, room: usize) -> [usize; 8] {
        self.effects.clear();
        let effects = &mut self.effects;
        let view = self.side.acquire(room, |e| effects.push(*e)).unwrap();
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

    /// Each lane's selected frame: its group's quad, 10 wide at
    /// `x = 100 · lane`, as an index from the frame the first frame read,
    /// before any judgment, drew.
    fn read(&mut self, uvs: &UvTable, quads: &[Quad]) -> [usize; 8] {
        let drawn: [usize; 8] = std::array::from_fn(|l| {
            let q: Vec<&Quad> = quads
                .iter()
                .filter(|q| q.rect[2].round() as i32 == 10 && q.rect[0].round() as usize == 100 * l)
                .collect();
            assert_eq!(q.len(), 1, "lane {l}");
            uvs.frames()
                .iter()
                .position(|f| f.uv == q[0].uv && f.layer == q[0].layer)
                .unwrap()
        });
        let zero = *self.zero.get_or_insert(drawn);
        std::array::from_fn(|l| drawn[l] - zero[l])
    }
}

/// Under the beatoraja-style preset, at the chart's judge rank: key 1 30
/// ms early, GREAT (2); a rearm (0 everywhere) and the scratch on time,
/// PERFECT (1), while key 1's note is missed (left as it is); key 1's
/// second 40 ms late, GREAT (3); 100 ms after it an empty POOR, late (11);
/// 300 ms before the third one, early (10); the third missed (left); a
/// press 400 ms after it, matching nothing (0); key 2's head 10 ms early,
/// held (8); its release 140 ms before its end, GOOD by the release,
/// early (4); key 3 100 ms late, GOOD (5), then 150 ms early, BAD (6);
/// key 4 40 ms early, GREAT (2), then a press on its note's time, an
/// empty POOR on time, which is late (11); the scratch's long note held
/// by its axis (8), then a reversal 100 ms before its end, whose release
/// judges it and whose press, at the same µs and after it, matches nothing
/// (0), and stays so through the release deadline after it; key 5's head
/// pressed and released in one µs, the release judging it BAD (6), which
/// stays; key 4 GREAT with its effects held back a frame, 0 until they
/// arrive, then 2. A retry starts at 0 and takes its own first judgment.
#[test]
fn each_lane_reaches_the_skin_through_logic_and_the_frame() {
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
    assert_eq!(r.step(&skin, &uvs, 500), [0; 8], "before any");
    r.tap(970, 1);
    assert_eq!(r.step(&skin, &uvs, 980), [0, 2, 0, 0, 0, 0, 0, 0], "GREAT");
    // A rearmed candidate: its view is the next generation, and the frame
    // starts the skin over.
    r.logic.rearm(&InputStart::default(), TimeUs(0));
    let rearmed = r.frame(&skin, &uvs, TimeUs(990 * MS), 0);
    assert_eq!(r.generation, 1);
    assert_eq!(rearmed, [0; 8], "rearmed");
    r.tap(1_000, 0);
    let s = r.step(&skin, &uvs, 1_300);
    assert_eq!(s, [1, 0, 0, 0, 0, 0, 0, 0], "PERFECT, a miss");
    r.tap(2_040, 1);
    let s = r.step(&skin, &uvs, 2_040);
    assert_eq!(s, [1, 3, 0, 0, 0, 0, 0, 0], "late GREAT");
    r.tap(2_100, 1);
    let s = r.step(&skin, &uvs, 2_100);
    assert_eq!(s, [1, 11, 0, 0, 0, 0, 0, 0], "late empty POOR");
    r.tap(2_700, 1);
    let s = r.step(&skin, &uvs, 2_700);
    assert_eq!(s, [1, 10, 0, 0, 0, 0, 0, 0], "early empty POOR");
    let s = r.step(&skin, &uvs, 3_300);
    assert_eq!(s, [1, 10, 0, 0, 0, 0, 0, 0], "miss");
    r.tap(3_400, 1);
    let s = r.step(&skin, &uvs, 3_400);
    assert_eq!(s, [1, 0, 0, 0, 0, 0, 0, 0], "matching nothing");
    r.set(3_990, 2, 1);
    let s = r.step(&skin, &uvs, 3_995);
    assert_eq!(s, [1, 0, 8, 0, 0, 0, 0, 0], "held");
    r.set(4_360, 2, 0);
    let s = r.step(&skin, &uvs, 4_370);
    assert_eq!(s, [1, 0, 4, 0, 0, 0, 0, 0], "released");
    r.tap(5_100, 3);
    let s = r.step(&skin, &uvs, 5_100);
    assert_eq!(s, [1, 0, 4, 5, 0, 0, 0, 0], "late GOOD");
    r.tap(5_350, 3);
    let s = r.step(&skin, &uvs, 5_350);
    assert_eq!(s, [1, 0, 4, 6, 0, 0, 0, 0], "early BAD");
    r.tap(5_960, 4);
    let s = r.step(&skin, &uvs, 5_960);
    assert_eq!(s, [1, 0, 4, 6, 2, 0, 0, 0], "early GREAT");
    r.tap(6_000, 4);
    let s = r.step(&skin, &uvs, 6_000);
    assert_eq!(s, [1, 0, 4, 6, 11, 0, 0, 0], "empty POOR on time");
    // The axis at rest, then +2, a press on the head's time.
    r.set(6_200, AXIS, 100);
    r.set(6_500, AXIS, 102);
    let s = r.step(&skin, &uvs, 6_500);
    assert_eq!(s, [8, 0, 4, 6, 11, 0, 0, 0], "scratch held");
    // Held by a step forward every 60 ms, inside the release time.
    for (k, ms) in (6_560..=6_860).step_by(60).enumerate() {
        r.set(ms, AXIS, 103 + i32::try_from(k).unwrap());
    }
    let s = r.step(&skin, &uvs, 6_880);
    assert_eq!(s, [8, 0, 4, 6, 11, 0, 0, 0], "still held");
    // Back by 2: a release, which judges the long note, then a press, in
    // one µs.
    r.set(6_900, AXIS, 106);
    let s = r.step(&skin, &uvs, 6_900);
    assert!(
        r.effects.iter().any(
            |e| matches!(e.kind, eval::JudgeEventKind::Judged { lane, .. } if lane == Lane::SCRATCH)
        ),
        "{:?}",
        r.effects
    );
    assert_eq!(s, [0, 0, 4, 6, 11, 0, 0, 0], "reversal");
    // Key 5's head pressed on its time and released in the same µs, the
    // other order: the release, after the press, judges the long note BAD
    // early (6), which stays.
    r.set(7_500, 5, 1);
    r.set(7_500, 5, 0);
    let s = r.step(&skin, &uvs, 7_500);
    assert_eq!(s, [0, 0, 4, 6, 11, 6, 0, 0], "press, release");
    let s = r.step(&skin, &uvs, 7_600);
    assert_eq!(s, [0, 0, 4, 6, 11, 6, 0, 0], "press, release, later");
    // Key 4 20 ms early, GREAT, its effects held back for a frame: the
    // view's press is ahead of them, so the lane reads 0 until they
    // arrive, then their code (2).
    r.tap(8_480, 4);
    r.writer.publish(TimeUs(8_480 * MS));
    let emitted = r.logic.advance_to(TimeUs(8_480 * MS)).emitted;
    assert!(emitted > 0);
    let s = r.bounded(&skin, &uvs, TimeUs(8_480 * MS), 0);
    assert_eq!(s, [0, 0, 4, 6, 0, 6, 0, 0], "effects pending");
    let s = r.frame(&skin, &uvs, TimeUs(8_490 * MS), emitted);
    assert_eq!(s, [0, 0, 4, 6, 2, 6, 0, 0], "effects delivered");
    // Key 6 pressed 200 ms late, judged BAD, then released and pressed
    // again in that µs, the second press past the empty POOR range: it
    // matches nothing and clears the BAD.
    r.set(9_200, 6, 1);
    r.set(9_200, 6, 0);
    r.set(9_200, 6, 1);
    let s = r.step(&skin, &uvs, 9_200);
    let lane6 = Lane::new(6).unwrap();
    assert!(
        r.effects.iter().any(|e| matches!(
            e.kind,
            eval::JudgeEventKind::Judged { lane, judgment: rules::Judgment::Bad, .. } if lane == lane6
        )),
        "{:?}",
        r.effects
    );
    assert_eq!(s, [0, 0, 4, 6, 2, 6, 0, 0], "BAD, release, press");
    r.set(9_210, 6, 0);
    // Key 7's head on time, held (8); 100 ms before its end a release that
    // judges it, an unmatched press and a release, all in one µs: the
    // press came after the judgment, so 0, though the lane ends released.
    r.set(9_500, 7, 1);
    let s = r.step(&skin, &uvs, 9_500);
    assert_eq!(s, [0, 0, 4, 6, 2, 6, 0, 8], "held");
    r.set(9_900, 7, 0);
    r.set(9_900, 7, 1);
    r.set(9_900, 7, 0);
    let s = r.step(&skin, &uvs, 9_900);
    assert_eq!(s, [0, 0, 4, 6, 2, 6, 0, 0], "release, press, release");
    // Key 7's next long note held through its end: the deadline judges it
    // PERFECT (1); a press after it clears it.
    r.set(10_500, 7, 1);
    let s = r.step(&skin, &uvs, 10_600);
    assert_eq!(s, [0, 0, 4, 6, 2, 6, 0, 8], "held again");
    let s = r.step(&skin, &uvs, 11_000);
    assert_eq!(s, [0, 0, 4, 6, 2, 6, 0, 1], "held to its end");
    r.set(11_100, 7, 0);
    let s = r.step(&skin, &uvs, 11_100);
    assert_eq!(s, [0, 0, 4, 6, 2, 6, 0, 1], "released after");
    r.tap(11_200, 7);
    let s = r.step(&skin, &uvs, 11_200);
    assert_eq!(s, [0, 0, 4, 6, 2, 6, 0, 0], "a press after it");
    // Key 6's two taps at 12 s, each judged by a press in that µs: the
    // second press's PERFECT, which counted both presses, stays (1).
    r.set(12_000, 6, 1);
    r.set(12_000, 6, 0);
    r.set(12_000, 6, 1);
    let s = r.step(&skin, &uvs, 12_000);
    let judged = r
        .effects
        .iter()
        .filter(|e| matches!(e.kind, eval::JudgeEventKind::Judged { lane, .. } if lane == lane6))
        .count();
    assert_eq!(judged, 2, "{:?}", r.effects);
    assert_eq!(s, [0, 0, 4, 6, 2, 6, 1, 0], "two presses, two judgments");
    // A retry: a fresh attempt and preparation.
    let mut r = Rig::new(&chart, &rule, &dev, &skin, &ctx);
    assert_eq!(r.step(&skin, &uvs, 500), [0; 8], "retried");
    r.tap(970, 1);
    let s = r.step(&skin, &uvs, 980);
    assert_eq!(s, [0, 2, 0, 0, 0, 0, 0, 0], "after the retry");
}
