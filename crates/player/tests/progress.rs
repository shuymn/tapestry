//! The play's progress on the production path, deterministically (docs/
//! stage3.md R3): input through real `Logic`, the render side's
//! acquisition and the production frame adapter (`render::skin_frame`)
//! into an imported skin's type-6 slider, each frame at a chosen time and
//! acknowledged before the next advance. The slider's x is the drawn
//! progress: 0 in READY, the share of the way from the attempt's start to
//! its last resolved object, 1 at and after it, through manual and
//! autoplay input, a practice speed, a rearm and a retry.

use bms_model::{Chart, Lane, Speed, TimeUs};
use eval::{
    Binding, ControlId, DeviceProfile, InputStart, JudgeEvent, PhysicalInputEvent, Section,
    SourceProfile,
};
use game::{Logic, LogicConfig, RenderSide};
use input::{SourceReader, SourceWriter, source_queue};
use render::FrameInput;
use skin_beatoraja::{Config, import, read_header};
use skin_model::{
    AtlasLimits, Attempt, FrameReadouts, GaugeDesc, LaneView, PlayContext, QuadBatch, Side, Skin,
    UvTable, Variant,
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

/// The slider's x at rest and its range: it moves right.
const AT_REST: f64 = 100.0;
const RANGE: f64 = 1_000.0;

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

/// The chart at `speed`: 120 BPM, key 1 at 2.0, 2.5, 3.0 and 3.5 s of the
/// 1× chart and a background sound at 4.0 s, its last object, which a
/// normal play and an autoplay alike count to (stage3.md D4).
fn chart(speed: Speed) -> Chart {
    let text = "#PLAYER 1\n#BPM 120\n#WAV01 key.wav\n#00111:01010101\n#00201:01\n";
    let opts = bms_parser::LoadOptions {
        speed,
        ..Default::default()
    };
    bms_parser::load_bms(text.as_bytes(), std::path::Path::new("."), &opts)
        .unwrap()
        .chart
}

/// The skin: a type-6 slider moving right by 1 000 from x 100, 10 wide.
fn skin(dir: &std::path::Path) -> (Skin, UvTable) {
    std::fs::write(
        dir.join("s.json"),
        r#"{"type": 0, "w": 1920, "h": 1080,
            "source": [{"id": "a", "path": "a.png"}],
            "slider": [{"id": "p", "src": "a", "w": 16, "h": 16, "type": 6, "angle": 1,
                        "range": 1000}],
            "destination": [{"id": "p", "dst": [{"x": 100, "y": 500, "w": 10, "h": 10}]}]}"#,
    )
    .unwrap();
    let file = std::fs::File::create(dir.join("a.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 16, 16);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let white: Vec<u8> = (0..16 * 16).flat_map(|_| [255; 4]).collect();
    e.write_header().unwrap().write_image_data(&white).unwrap();
    let (mut def, h) = read_header(dir, "s.json").unwrap();
    let imported = import(
        &mut def,
        &h,
        &Config::defaults(&h),
        skin_model::ScreenKind::Play,
    )
    .unwrap();
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
}

impl<'a> Rig<'a> {
    fn new(
        chart: &'a Chart,
        rule: &'a rules::RulePreset,
        dev: &'a DeviceProfile,
        skin: &Skin,
        ctx: &PlayContext<'_>,
        section: Option<&Section>,
    ) -> Self {
        let (writer, reader) = source_queue(64);
        let (logic, side) = Logic::with_section(
            chart,
            rule,
            dev,
            &InputStart::default(),
            section,
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
        }
    }

    /// Publishes the source up to `ms`, advances the logic there, and
    /// returns the slider's x in the frame drawn at `ms`.
    fn step(&mut self, skin: &Skin, uvs: &UvTable, ms: i64) -> f64 {
        let now = TimeUs(ms * MS);
        self.writer.publish(now);
        let emitted = self.logic.advance_to(now).emitted;
        self.frame(skin, uvs, now, emitted)
    }

    /// The slider's x in the frame at `now` from the render side's
    /// acquisition, rearming the skin when the view is a new candidate's,
    /// as the render thread does; `emitted` effects are expected.
    fn frame(&mut self, skin: &Skin, uvs: &UvTable, now: TimeUs, emitted: usize) -> f64 {
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
        let [q] = self.batch.quads() else {
            panic!("one slider quad: {:?}", self.batch.quads());
        };
        assert_eq!([q.rect[1], q.rect[2]], [570.0, 10.0]);
        f64::from(q.rect[0])
    }
}

/// The slider's x at `p` of the whole progress.
fn at(p: f64) -> f64 {
    AT_REST + RANGE * p
}

#[track_caller]
fn assert_near(got: f64, want: f64, what: &str) {
    assert!((got - want).abs() < 1e-3, "{what}: {got}, not {want}");
}

/// The play context of an attempt: the whole chart after a READY of
/// `lead_in`, or `section`.
fn context<'a>(
    gauges: &'a [GaugeDesc],
    rule: &rules::RulePreset,
    autoplay: bool,
    attempt: Attempt<'a>,
) -> PlayContext<'a> {
    PlayContext {
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay,
        gauges,
        attempt,
        empty_poor_breaks_combo: rule.empty_poor.breaks_combo,
        empty_poor_counted: rule.empty_poor.counted,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(i64::MAX),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
    }
}

/// A whole-chart play, manual (key 1 hit on time) and autoplay (the
/// autoplay input of the production profile): READY reads 0, the slider
/// moves with the play clock to the last object at 4 s, a quarter at 1 s,
/// and stays whole after it; judgments move nothing. A rearmed candidate's
/// READY reads 0 again, and a retry's fresh attempt starts at 0.
#[test]
fn the_progress_reaches_the_skin_through_logic_and_the_frame() {
    let dir = tempfile::tempdir().unwrap();
    let (skin, uvs) = skin(dir.path());
    let chart = chart(Speed::NORMAL);
    let rule = rules::beatoraja_style();
    let dev = device();
    let gauges: Vec<GaugeDesc> = rule.gauges.iter().map(GaugeDesc::from_rule).collect();
    let full = Attempt::Full {
        lead_in: TimeUs(1_000 * MS),
    };
    for autoplay in [false, true] {
        let ctx = context(&gauges, &rule, autoplay, full);
        let mut r = Rig::new(&chart, &rule, &dev, &skin, &ctx, None);
        let input: Vec<PhysicalInputEvent> = if autoplay {
            input::autoplay(&chart, &dev.sources[0]).unwrap()
        } else {
            [2_000, 2_500, 3_000, 3_500]
                .iter()
                .flat_map(|&ms| [(ms, 1), (ms + 50, 0)])
                .map(|(ms, value)| PhysicalInputEvent {
                    received: TimeUs(ms * MS),
                    source: eval::SourceId(0),
                    kind: eval::InputKind::Control {
                        control: ControlId(1),
                        value,
                    },
                })
                .collect()
        };
        assert_eq!(input.len(), 8, "{autoplay}");
        for e in input {
            r.writer.push(e).unwrap();
        }
        let what = |w: &str| format!("{w} (autoplay {autoplay})");
        let ready = r.frame(&skin, &uvs, TimeUs(-500 * MS), 0);
        assert_near(ready, at(0.0), &what("READY"));
        for (ms, p) in [
            (0, 0.0),
            (1_000, 0.25),
            (2_100, 0.525),
            (3_000, 0.75),
            (4_000, 1.0),
            (6_000, 1.0),
        ] {
            assert_near(r.step(&skin, &uvs, ms), at(p), &what(&format!("{ms} ms")));
        }
        assert!(
            r.logic.view().tally.counts.perfect > 0,
            "{autoplay}: nothing hit"
        );
        // A rearmed candidate: its view is the next generation, its READY
        // before chart time 0.
        r.logic.rearm(&InputStart::default(), TimeUs(0));
        let rearmed = r.frame(&skin, &uvs, TimeUs(-300 * MS), 0);
        assert_eq!(r.generation, 1);
        assert_near(rearmed, at(0.0), &what("rearmed"));
        assert_near(
            r.step(&skin, &uvs, 1_000),
            at(0.25),
            &what("after the rearm"),
        );
        // A retry: a fresh attempt and preparation.
        let mut r = Rig::new(&chart, &rule, &dev, &skin, &ctx, None);
        assert_near(
            r.frame(&skin, &uvs, TimeUs(-500 * MS), 0),
            at(0.0),
            &what("retried"),
        );
        assert_near(
            r.step(&skin, &uvs, 2_000),
            at(0.5),
            &what("after the retry"),
        );
    }
}

/// A practice from 2 s to the chart's end at half speed: the chart loaded
/// at the speed, the section the game makes of it in real time, so the
/// progress takes twice as long and reads the same share at the same
/// chart position (a practice speed scales real time, not the ratio).
/// The lead-in reads 0, and the end of the section 1.
#[test]
fn a_practice_speed_scales_the_time_not_the_share() {
    let dir = tempfile::tempdir().unwrap();
    let (skin, uvs) = skin(dir.path());
    let rule = rules::beatoraja_style();
    let dev = device();
    let gauges: Vec<GaugeDesc> = rule.gauges.iter().map(GaugeDesc::from_rule).collect();
    let mut shares = Vec::new();
    for speed in [Speed::NORMAL, Speed::SLOWEST] {
        let chart = chart(speed);
        let practice = game::mid_chart_start(&chart, speed, &rule, TimeUs(2_000 * MS));
        let section = practice.section(speed);
        let k = i64::from(20 / speed.twentieths());
        assert_eq!(section.start, TimeUs(2_000 * MS * k), "{speed:?}");
        let ctx = context(&gauges, &rule, false, Attempt::Section(&section));
        let mut r = Rig::new(&chart, &rule, &dev, &skin, &ctx, Some(&section));
        let lead_in = r.frame(
            &skin,
            &uvs,
            TimeUs(section.start.0 - section.lead_in.0 / 2),
            0,
        );
        assert_near(lead_in, at(0.0), &format!("{speed:?}: the lead-in"));
        let (start, end) = (section.start.0, section.end.0);
        let span = (end - start) as f64;
        // The chart's 2.75 s, halfway through the 1× section, at each speed.
        let half = start + 750 * MS * k;
        let p = (half - start) as f64 / span;
        let x = r.step(&skin, &uvs, half / MS);
        assert_near(x, at(p), &format!("{speed:?}: halfway"));
        shares.push((half - start, p));
        assert_near(
            r.step(&skin, &uvs, end / MS + 1),
            at(1.0),
            &format!("{speed:?}: the end"),
        );
    }
    // Twice the real time to the same share.
    assert_eq!(shares[1].0, 2 * shares[0].0);
    assert!((shares[0].1 - shares[1].1).abs() < 1e-5, "{shares:?}");
    assert!((shares[0].1 - 0.5).abs() < 1e-5, "{shares:?}");
}
