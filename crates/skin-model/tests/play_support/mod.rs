//! Helpers shared by the play tests: effects, snapshots and a harness that
//! builds frames.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bms_model::{Chart, Lane, NoteKind, TimeUs};
use eval::{GaugeState, JudgeEvent, JudgeEventKind, NoteFacts, NoteHold, Tally};
use image::{ImageFormat, Rgba, RgbaImage};
use rules::{GaugeValue, Judgment, PerJudgment};
use skin_model::{
    Anchor, AtlasLimits, Attempt, Blend, BuildError, CoverSettings, Ease, ElementDef, End, FileDef,
    Filter, Fit, FrameReadouts, Gain, GaugeDesc, GaugeLook, ImageDef, KeyDef, LaneView, Layout,
    LoadError, PLAY, Place, Play, PlayContext, Program, QuadBatch, Region, RegionDef, Scope, Sheet,
    Side, Skin, SkinDef, SkinFrame, Source, TimerRef, Ty, UvTable, Variant, compile,
};

pub const MS: i64 = 1_000;

pub fn lane(n: u8) -> Lane {
    Lane::new(n).unwrap()
}

pub fn ev(time_ms: i64, kind: JudgeEventKind) -> JudgeEvent {
    JudgeEvent {
        seq: 0,
        time: TimeUs(time_ms * MS),
        origin: None,
        kind,
    }
}

pub fn judged(
    l: u8,
    note: usize,
    judgment: Judgment,
    deviation_ms: Option<i64>,
    release_ms: Option<i64>,
    combo: u32,
) -> JudgeEventKind {
    JudgeEventKind::Judged {
        lane: lane(l),
        note,
        judgment,
        deviation: deviation_ms.map(|d| TimeUs(d * MS)),
        release: release_ms.map(|r| TimeUs(r * MS)),
        combo,
        presses: 0,
    }
}

/// What the evaluator's note facts become after `ev`, for tests that
/// drive frames with effects: a judged tap or a hit mine is done, a long
/// note's head press before its end (held, or a BAD head) takes it and
/// starts the lane's hold, and its hold's end or its completion ends it. Drops are modelled
/// by changing the facts without effects.
pub fn follow(notes: &mut NoteFacts, chart: &Chart, ev: &JudgeEvent) {
    let end_of = |lane: Lane, note: usize| match chart.lanes[lane.index()].get(note)?.kind {
        NoteKind::Long { end, .. } => Some(end),
        NoteKind::Tap | NoteKind::Mine { .. } => None,
    };
    match ev.kind {
        JudgeEventKind::Judged {
            lane,
            note,
            deviation,
            release,
            ..
        } => {
            notes.set_done(lane, note);
            let Some(end) = end_of(lane, note) else {
                return;
            };
            if deviation.is_some() && release.is_none() && ev.time < end {
                notes.set_hold(
                    lane,
                    Some(NoteHold {
                        note,
                        start: ev.time,
                        end,
                    }),
                );
            } else if notes.hold(lane).is_some_and(|h| h.note == note) {
                // Its completion ends its hold.
                notes.set_hold(lane, None);
            }
        }
        JudgeEventKind::LnHeld { lane, note, .. } => {
            notes.set_done(lane, note);
            if let Some(end) = end_of(lane, note) {
                notes.set_hold(
                    lane,
                    Some(NoteHold {
                        note,
                        start: ev.time,
                        end,
                    }),
                );
            }
        }
        JudgeEventKind::LnHoldEnded { lane, note } => {
            if notes.hold(lane).is_some_and(|h| h.note == note) {
                notes.set_hold(lane, None);
            }
        }
        JudgeEventKind::MineHit { lane, note } => notes.set_done(lane, note),
        JudgeEventKind::EmptyPoor { .. }
        | JudgeEventKind::PlaySound { .. }
        | JudgeEventKind::Gauge { .. }
        | JudgeEventKind::SongEnd => {}
    }
}

pub fn ln_held(l: u8, note: usize, judgment: Judgment) -> JudgeEventKind {
    JudgeEventKind::LnHeld {
        lane: lane(l),
        note,
        judgment,
        deviation: TimeUs(0),
        presses: 0,
    }
}

pub fn hold_ended(l: u8, note: usize) -> JudgeEventKind {
    JudgeEventKind::LnHoldEnded {
        lane: lane(l),
        note,
    }
}

pub fn mine(l: u8, note: usize) -> JudgeEventKind {
    JudgeEventKind::MineHit {
        lane: lane(l),
        note,
    }
}

pub fn empty_poor(l: u8) -> JudgeEventKind {
    JudgeEventKind::EmptyPoor {
        lane: lane(l),
        deviation: TimeUs(0),
        presses: 0,
    }
}

pub fn gauge(g: usize, value: GaugeValue) -> JudgeEventKind {
    JudgeEventKind::Gauge {
        gauge: g,
        value,
        reference: value,
    }
}

pub fn tally(combo: u32) -> Tally {
    Tally {
        counts: PerJudgment {
            perfect: 0,
            great: 0,
            good: 0,
            bad: 0,
            poor: 0,
        },
        ex_score: 0,
        bp: 0,
        combo,
        max_combo: combo,
        empty_poor: 0,
        ..Default::default()
    }
}

pub fn state(value: GaugeValue) -> GaugeState {
    GaugeState {
        value,
        reference: value,
        failed_at: None,
    }
}

pub fn pct(p: i64) -> GaugeValue {
    GaugeValue(p * GaugeValue::PERCENT)
}

/// Two gauges, both starting at 20 % with a 100 % maximum.
pub const GAUGES: [GaugeDesc; 2] = [
    GaugeDesc {
        start: GaugeValue(20 * GaugeValue::PERCENT),
        max: GaugeValue::FULL,
        look: GaugeLook::Normal,
        border: GaugeValue(80 * GaugeValue::PERCENT),
    },
    GaugeDesc {
        start: GaugeValue(20 * GaugeValue::PERCENT),
        max: GaugeValue::FULL,
        look: GaugeLook::Normal,
        border: GaugeValue(80 * GaugeValue::PERCENT),
    },
];

/// A notes' end past every frame these tests build, so `last_note` stays
/// off unless a test sets its own.
pub const NO_END: TimeUs = TimeUs(3_600_000 * MS);

pub fn full(lead_in_ms: i64) -> PlayContext<'static> {
    PlayContext {
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: &GAUGES,
        attempt: Attempt::Full {
            lead_in: TimeUs(lead_in_ms * MS),
        },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: NO_END,
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
        score: skin_model::PlayScore::NONE,
    }
}

/// The readouts of a frame showing `view`: no tempo, hi-speed or duration,
/// and every cover chosen on, stored at the amount `view` draws (capped
/// at 1 000), so the frame is consistent.
pub fn readouts_for(view: LaneView) -> FrameReadouts {
    FrameReadouts {
        covers: CoverSettings {
            cover: view.cover.min(1_000),
            lift: view.lift.min(1_000),
            hidden: view.hidden.min(1_000),
            cover_enabled: true,
            lift_enabled: true,
            hidden_enabled: true,
            adjusting: false,
        },
        ..FrameReadouts::UNAVAILABLE
    }
}

pub fn bool_program(src: &str) -> Program {
    compile(src, &Scope::new(&PLAY), Ty::Bool).unwrap()
}

/// The limits the play tests build atlases with.
pub const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 256,
    max_layers: 4,
    reserved_layers: 1,
};

/// A folder holding `white.png`, one opaque white texel, under the
/// target's test folder; written once, atomically.
fn white_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("skin-model-play");
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = tempfile::Builder::new()
            .suffix(".png")
            .tempfile_in(&dir)
            .unwrap();
        RgbaImage::from_pixel(1, 1, Rgba([255; 4]))
            .save_with_format(tmp.path(), ImageFormat::Png)
            .unwrap();
        // Under nextest each test is a process of its own, so another
        // may write the same file at once. On Windows the rename over a
        // file another process has open fails; the file there has the
        // same bytes, so it is used.
        let path = dir.join("white.png");
        if let Err(e) = tmp.persist(&path)
            && !path.is_file()
        {
            panic!("{path:?}: {e}");
        }
        dir
    })
}

/// A sheet with one image: a white texel.
pub fn white_sheet() -> Sheet {
    Sheet::new(
        white_root(),
        vec![FileDef {
            path: "white.png".into(),
            hi: None,
        }],
        vec![ImageDef {
            file: 0,
            rect: None,
            grid: [1, 1],
            cycle: None,
            filter: Filter::Linear,
        }],
    )
    .unwrap()
}

/// Every region anchored top left, none optional.
pub const REGIONS: [RegionDef; Region::COUNT] = [RegionDef {
    anchor: Anchor::TopLeft,
    optional: false,
}; Region::COUNT];

/// An adapted skin without lanes, its regions [`REGIONS`].
pub fn def(whens: Vec<Program>, sheet: Sheet, elements: Vec<ElementDef>) -> SkinDef {
    SkinDef {
        fit: Fit::Adapt,
        regions: REGIONS,
        whens,
        sheet,
        lanes: None,
        elements,
        widgets: skin_model::WidgetDefs::default(),
        bomb_up_to: Some(skin_model::Grade::Bad),
        close: None,
        loadend: None,
        present: None,
    }
}

/// A skin over [`white_sheet`].
pub fn load(whens: Vec<Program>, elements: Vec<ElementDef>) -> Result<Skin, LoadError> {
    Skin::new(def(whens, white_sheet(), elements))
}

/// The gain that shows 600 ms over the whole lane at 150 BPM: `H` 8/3,
/// as `240000 / (150 · 600)`.
pub const GAIN_600_MS_AT_150: Option<Gain> = Gain::new(8, 3);

/// What a frame shows the lanes by default: 600 ms over the lane at 150
/// BPM, no covers.
pub const VIEW: LaneView = LaneView {
    gain: GAIN_600_MS_AT_150,
    cover: 0,
    lift: 0,
    hidden: 0,
};

/// The uv table of `skin`'s sheet at [`LIMITS`], without a text layer.
pub fn uvs(skin: &Skin) -> UvTable {
    skin.sheet()
        .atlas(Variant::Lo, LIMITS, &|| false)
        .unwrap()
        .into_parts()
        .1
}

/// An element at x = `x` with a hold track from 0, on `timer`, showing
/// image 0.
pub fn element(timer: TimerRef, x: i32) -> ElementDef {
    let mut props = [None; 9];
    props[0] = Some(x);
    ElementDef {
        source: Source::Image(0),
        track: skin_model::Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props,
            }],
            Ease::Linear,
            End::Hold,
        )
        .unwrap(),
        timer,
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    }
}

/// A skin, its atlas's uv table, its play, the snapshot it reads and a
/// batch.
pub struct Harness {
    pub skin: Skin,
    pub uvs: UvTable,
    pub play: Play,
    pub tally: Tally,
    pub gauges: Vec<GaugeState>,
    pub gauge: usize,
    pub offset: TimeUs,
    pub batch: QuadBatch,
    pub complete: bool,
    pub playing: bool,
    /// Where the play entered Closing ([`SkinFrame::closing`]).
    pub closing: Option<TimeUs>,
    pub target: [u32; 2],
    pub layout: Layout,
    pub view: LaneView,
    /// The frames' readouts; `None` gives [`readouts_for`] the view.
    pub readouts: Option<FrameReadouts>,
    /// The frames' turntable angle ([`SkinFrame::turntable`]).
    pub turntable: i32,
    /// The state clock each frame gives: by default 0.
    pub state_us: i64,
    /// The chart, which [`follow`] reads.
    pub chart: Chart,
    /// The note facts each frame gives, following its effects
    /// ([`follow`]) unless a test changes them.
    pub notes: NoteFacts,
    /// The lane input each frame gives.
    pub input: eval::InputFacts,
    /// The judgment offsets each frame gives: the slots and the count
    /// ([`Harness::record`]).
    pub ring: [i32; skin_model::OFFSET_SLOTS],
    pub ring_count: u32,
}

impl Harness {
    pub fn new(skin: Skin, chart: &Chart, ctx: &PlayContext<'_>) -> Self {
        let play = skin.prepare(chart, ctx).unwrap();
        let batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        Self {
            uvs: uvs(&skin),
            skin,
            play,
            tally: tally(0),
            gauges: vec![state(GAUGES[0].start), state(GAUGES[1].start)],
            gauge: 0,
            offset: TimeUs(0),
            batch,
            complete: false,
            playing: true,
            closing: None,
            target: [1_920, 1_080],
            layout: Layout::AUTHOR,
            view: VIEW,
            readouts: None,
            turntable: 0,
            state_us: 0,
            chart: chart.clone(),
            notes: NoteFacts::new(chart),
            input: eval::InputFacts::NONE,
            ring: [0; skin_model::OFFSET_SLOTS],
            ring_count: 0,
        }
    }

    /// Records `ms` in the ring the frames give, as the play view's ring
    /// records a judgment's offset.
    pub fn record(&mut self, ms: i32) {
        let slot = usize::try_from(self.ring_count).unwrap() % skin_model::OFFSET_SLOTS;
        self.ring[slot] = ms;
        self.ring_count += 1;
    }

    /// Clears the ring, as a rearm does.
    pub fn clear_ring(&mut self) {
        self.ring = [0; skin_model::OFFSET_SLOTS];
        self.ring_count = 0;
    }

    /// An empty skin over `chart`.
    pub fn empty(chart: &Chart, ctx: &PlayContext<'_>) -> Self {
        Self::new(load(Vec::new(), Vec::new()).unwrap(), chart, ctx)
    }

    /// Builds the frame at `now_us` with `effects`, which must build.
    pub fn frame_us(&mut self, now_us: i64, effects: &[JudgeEvent]) {
        self.try_frame_us(now_us, effects)
            .expect("the frame builds");
    }

    /// Builds the frame at `now_us` with `effects`.
    pub fn try_frame_us(&mut self, now_us: i64, effects: &[JudgeEvent]) -> Result<(), BuildError> {
        self.batch.clear();
        for ev in effects {
            follow(&mut self.notes, &self.chart, ev);
        }
        let frame = SkinFrame {
            state_us: self.state_us,
            now: TimeUs(now_us),
            display_offset: self.offset,
            tally: &self.tally,
            gauges: &self.gauges,
            gauge: self.gauge,
            effects,
            notes: &self.notes,
            offsets: skin_model::Offsets {
                slots: &self.ring,
                count: self.ring_count,
            },
            input: &self.input,
            complete: self.complete,
            target: self.target,
            layout: &self.layout,
            turntable: self.turntable,
            lanes: self.view,
            playing: self.playing,
            closing: self.closing,
            readouts: self.readouts.unwrap_or_else(|| readouts_for(self.view)),
        };
        self.skin
            .build(&mut self.play, &frame, &self.uvs, &mut self.batch)
    }

    /// Builds the frame at `now_ms` with `effects`.
    pub fn frame(&mut self, now_ms: i64, effects: &[JudgeEvent]) {
        self.frame_us(now_ms * MS, effects);
    }

    /// Timer `slot`'s start in µs.
    pub fn start(&self, slot: u16) -> Option<i64> {
        self.play.timer(slot).map(|t| t.0)
    }

    /// Timer `slot`'s start in ms (exact ms expected).
    pub fn start_ms(&self, slot: u16) -> Option<i64> {
        self.start(slot).map(|t| {
            assert_eq!(t % MS, 0, "slot {slot} starts off the ms");
            t / MS
        })
    }

    /// Timer `slot`'s reading in ms.
    pub fn elapsed(&self, slot: u16) -> i32 {
        self.play.state().timers[usize::from(slot)]
    }

    pub fn field(&self, slot: u16) -> i32 {
        self.play.state().fields[usize::from(slot)]
    }
}
