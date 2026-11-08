//! Drawing an imported skin: its atlas, a play of an empty chart, and
//! frames built at chosen times and play states.

#![allow(dead_code)]

use std::collections::HashMap;

use bms_model::{Chart, ChartBuilder, Lane, LnMode, NoteKind, TimeUs};
use eval::{GaugeState, JudgeEvent, JudgeEventKind, NoteFacts, NoteHold, Tally};
use rules::{GaugeValue, Judgment, PerJudgment};
use skin_model::{
    Atlas, AtlasLimits, Attempt, BpmRange, ChartText, CoverSettings, FrameReadouts, FrameUv, Gain,
    GaugeDesc, GaugeLook, LaneView, Layout, LoadState, Loading, LoadingFrame, LoadingGauge,
    PlayContext, Quad, QuadBatch, Side, Skin, SkinFrame, TextImage, TextTable, UvTable, Variant,
};

pub const MS: i64 = 1_000;

/// A notes' end past every frame the tests build: timer 143 stays off.
pub const NO_END: TimeUs = TimeUs(3_600_000 * MS);

#[allow(dead_code, reason = "not every test binary reads them")]
pub const GAUGES: [GaugeDesc; 1] = [desc(GaugeLook::Normal)];

const fn desc(look: GaugeLook) -> GaugeDesc {
    GaugeDesc {
        start: GaugeValue(20 * GaugeValue::PERCENT),
        max: GaugeValue::FULL,
        look,
        border: GaugeValue(80 * GaugeValue::PERCENT),
    }
}

/// One gauge of each look, in [`GaugeLook::ALL`]'s order.
const LOOKS: [GaugeDesc; GaugeLook::COUNT] = [
    desc(GaugeLook::AssistEasy),
    desc(GaugeLook::Easy),
    desc(GaugeLook::Normal),
    desc(GaugeLook::Hard),
    desc(GaugeLook::ExHard),
    desc(GaugeLook::Hazard),
];

pub const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 4_096,
    max_layers: 8,
    reserved_layers: 1,
};

/// The readouts of a frame showing `view`: no tempo, hi-speed or duration,
/// and every cover chosen on, stored at the amount `view` draws (capped
/// at 1 000).
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

pub fn tally(ex_score: u32, combo: u32) -> Tally {
    Tally {
        counts: PerJudgment {
            perfect: 0,
            great: 0,
            good: 0,
            bad: 0,
            poor: 0,
        },
        ex_score,
        bp: 0,
        combo,
        max_combo: combo,
        empty_poor: 0,
        ..Default::default()
    }
}

/// A judgment on lane 1 at `ms` with the event combo `combo`.
pub fn judged(ms: i64, judgment: Judgment, combo: u32) -> JudgeEvent {
    JudgeEvent {
        seq: 0,
        time: TimeUs(ms * MS),
        origin: None,
        kind: JudgeEventKind::Judged {
            lane: Lane::new(1).unwrap(),
            note: 0,
            judgment,
            deviation: Some(TimeUs(0)),
            release: None,
            combo,
            presses: 0,
        },
    }
}

/// An imported skin being played over a chart of `notes` notes.
pub struct Drawn {
    pub skin: Skin,
    pub uvs: UvTable,
    pub play: skin_model::Play,
    pub tally: Tally,
    pub gauges: Vec<GaugeState>,
    /// Whether the play is still playing, for every frame
    /// ([`SkinFrame::playing`]): off in its audio tail.
    pub playing: bool,
    /// Where the play entered Closing, for every frame
    /// ([`SkinFrame::closing`]).
    pub closing: Option<TimeUs>,
    pub batch: QuadBatch,
    /// The hi-speed and covers each frame shows.
    pub view: LaneView,
    /// The render target's size each frame draws at.
    pub target: [u32; 2],
    /// The readouts each frame gives; `None` gives [`readouts_for`] the
    /// view.
    pub readouts: Option<FrameReadouts>,
    /// The chart played, which [`follow`] reads.
    pub chart: Chart,
    /// The note facts each frame gives, following its effects
    /// ([`follow`]).
    pub notes: NoteFacts,
    /// The lane input each frame gives.
    pub input: eval::InputFacts,
    /// The turntable angle each frame gives: by default 0.
    pub turntable: i32,
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

/// The attempt every drawn import plays: the whole chart, no lead-in.
pub fn context() -> PlayContext<'static> {
    context_with(None, None)
}

/// [`context`] with the chart's BPM range `bpm_range` and BPM-change fact
/// `bpm_whole_change`.
fn context_with(
    bpm_range: Option<BpmRange>,
    bpm_whole_change: Option<bool>,
) -> PlayContext<'static> {
    PlayContext {
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: &GAUGES,
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: NO_END,
        bpm_range,
        bpm_whole_change,
        play_level: None,
    }
}

/// What a play of `skin` over `chart` keeps besides the atlas: its
/// prepared state and its batch.
pub fn prepared(skin: &Skin, chart: &bms_model::Chart) -> (skin_model::Play, QuadBatch) {
    let play = skin.prepare(chart, &context()).unwrap();
    let batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    (play, batch)
}

/// A chart with `notes` notes on lane 1, one every 10 ms from 1 s.
#[allow(dead_code, reason = "not every test binary builds one")]
pub fn chart(notes: u32) -> bms_model::Chart {
    (0..notes)
        .fold(ChartBuilder::new(150_000), |b, i| {
            b.tap(
                Lane::new(1).unwrap(),
                TimeUs(1_000_000 + i64::from(i) * 10_000),
            )
        })
        .build()
        .unwrap()
}

impl Drawn {
    pub fn new(skin: Skin, notes: u32) -> Self {
        Self::over(skin, &chart(notes))
    }

    /// A fresh attempt over a chart of `notes` notes, as a retry prepares
    /// it.
    #[allow(dead_code, reason = "not every test binary retries")]
    pub fn retry(&mut self, notes: u32) {
        self.chart = chart(notes);
        self.notes = NoteFacts::new(&self.chart);
        self.play = self.skin.prepare(&self.chart, &context()).unwrap();
    }

    /// `skin` played over a chart of `notes` notes with one gauge of
    /// look `look`.
    pub fn with_look(skin: Skin, notes: u32, look: GaugeLook) -> Self {
        let mut d = Self::new(skin, notes);
        let gauges = LOOKS
            .iter()
            .position(|g| g.look == look)
            .map(|i| &LOOKS[i..=i])
            .unwrap();
        let ctx = PlayContext {
            gauges,
            ..context()
        };
        d.play = d.skin.prepare(&d.chart, &ctx).unwrap();
        d
    }

    /// `skin` played over `chart`, its notes' end at `last_note_end`.
    #[allow(dead_code, reason = "not every test binary ends its notes")]
    pub fn over_ending(skin: Skin, chart: &bms_model::Chart, last_note_end: TimeUs) -> Self {
        Self::over_context(
            skin,
            chart,
            &PlayContext {
                last_note_end,
                ..context_with(None, None)
            },
        )
    }

    /// `skin` played over a chart of `notes` notes by an attempt that is
    /// autoplay when `autoplay`.
    #[allow(dead_code, reason = "not every test binary autoplays")]
    pub fn with_autoplay(skin: Skin, notes: u32, autoplay: bool) -> Self {
        let mut d = Self::new(skin, notes);
        let ctx = PlayContext {
            autoplay,
            ..context()
        };
        d.play = d.skin.prepare(&d.chart, &ctx).unwrap();
        d
    }

    /// `skin` played over `chart`.
    pub fn over(skin: Skin, chart: &bms_model::Chart) -> Self {
        Self::over_with(skin, chart, None, None)
    }

    /// `skin` played over `chart` whose play level is `play_level`.
    pub fn over_level(skin: Skin, chart: &bms_model::Chart, play_level: Option<i32>) -> Self {
        Self::over_context(
            skin,
            chart,
            &PlayContext {
                play_level,
                ..context_with(None, None)
            },
        )
    }

    /// `skin` played over `chart` whose metadata is `meta`.
    pub fn over_meta(
        skin: Skin,
        chart: &bms_model::Chart,
        meta: skin_model::ChartMetadata,
    ) -> Self {
        Self::over_context(
            skin,
            chart,
            &PlayContext {
                meta,
                ..context_with(None, None)
            },
        )
    }

    /// `skin` played over `chart` whose BPM range is `bpm_range` and
    /// BPM-change fact `bpm_whole_change`.
    pub fn over_with(
        skin: Skin,
        chart: &bms_model::Chart,
        bpm_range: Option<BpmRange>,
        bpm_whole_change: Option<bool>,
    ) -> Self {
        Self::over_context(skin, chart, &context_with(bpm_range, bpm_whole_change))
    }

    /// `skin` played over `chart` with the context `ctx`.
    fn over_context(skin: Skin, chart: &bms_model::Chart, ctx: &PlayContext<'_>) -> Self {
        let uvs = skin
            .atlas(Variant::Lo, LIMITS, &|| false)
            .unwrap()
            .into_parts()
            .1;
        let play = skin.prepare(chart, ctx).unwrap();
        let batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        Self {
            skin,
            uvs,
            play,
            tally: tally(0, 0),
            gauges: vec![GaugeState {
                value: GAUGES[0].start,
                reference: GAUGES[0].start,
                failed_at: None,
            }],
            playing: true,
            closing: None,
            batch,
            // 600 ms of 150 BPM over the original lane.
            view: LaneView {
                gain: Gain::new(8, 3),
                ..LaneView::NONE
            },
            target: [1_920, 1_080],
            readouts: None,
            chart: chart.clone(),
            notes: NoteFacts::new(chart),
            input: eval::InputFacts::NONE,
            turntable: 0,
        }
    }

    /// Attaches one line per text slot, each an ink box `w`×`h` ems
    /// advancing `w`.
    pub fn attach_lines(&mut self, w: f32, h: f32) {
        let chart = ChartText {
            title: "Title",
            subtitle: "",
            artist: "Artist",
            subartist: "",
            genre: "Genre",
        };
        let reqs = self
            .skin
            .text_requests(&chart, [1_920, 1_080], &Layout::AUTHOR);
        let mut lines = vec![None; self.skin.text_slots()];
        for r in reqs {
            let uv = FrameUv {
                uv: [0.0, 0.0, 0.5, 0.5],
                layer: 1,
            };
            lines[r.slot as usize] = TextImage::new(uv, [0.0, 0.0, w, h], w);
        }
        self.skin
            .attach_text(&mut self.play, TextTable::new(lines))
            .unwrap();
    }

    /// Lays out the bitmap lines for a chart titled `title`.
    pub fn attach_bitmap(&mut self, title: &str) {
        let chart = ChartText {
            title,
            ..ChartText::default()
        };
        self.skin.attach_bitmap_text(&mut self.play, &chart);
    }

    /// The quads of the frame at `ms` with `effects`.
    pub fn frame(&mut self, ms: i64, effects: &[JudgeEvent]) -> Vec<Quad> {
        self.build_frame(ms, effects);
        self.batch.quads().to_vec()
    }

    /// Builds the frame at `ms` with `effects` into the batch.
    fn build_frame(&mut self, ms: i64, effects: &[JudgeEvent]) {
        self.batch.clear();
        for ev in effects {
            follow(&mut self.notes, &self.chart, ev);
        }
        let frame = SkinFrame {
            now: TimeUs(ms * MS),
            display_offset: TimeUs(0),
            tally: &self.tally,
            gauges: &self.gauges,
            gauge: 0,
            effects,
            notes: &self.notes,
            input: &self.input,
            complete: false,
            target: self.target,
            layout: &Layout::AUTHOR,
            turntable: self.turntable,
            lanes: self.view,
            playing: self.playing,
            closing: self.closing,
            readouts: self.readouts.unwrap_or_else(|| readouts_for(self.view)),
        };
        self.skin
            .build(&mut self.play, &frame, &self.uvs, &mut self.batch)
            .expect("the frame builds");
    }

    /// Each digit by the uv of its glyph, read from the frames at `ms`
    /// with the EX score at 0 to 9, its number the lowest on the canvas;
    /// the tally is restored.
    pub fn ex_glyphs(&mut self, ms: i64) -> HashMap<[u32; 4], char> {
        let tally = self.tally;
        let mut glyph = HashMap::new();
        for digit in 0..10_u32 {
            self.tally.ex_score = digit;
            let q = self.frame(ms, &[]);
            let ex = q
                .iter()
                .max_by(|a, b| a.rect[1].total_cmp(&b.rect[1]))
                .unwrap();
            glyph.insert(
                ex.uv.map(f32::to_bits),
                char::from_digit(digit, 10).unwrap(),
            );
        }
        self.tally = tally;
        glyph
    }

    /// The rects, rounded, of the frame at `ms`.
    pub fn rects(&mut self, ms: i64) -> Vec<[i32; 4]> {
        self.build_frame(ms, &[]);
        self.batch.quads().iter().map(|q| round(q.rect)).collect()
    }

    /// The rects, rounded, of the frame at `ms`, each with its run's
    /// inline clip rect, rounded, when one.
    pub fn clipped(&mut self, ms: i64) -> Vec<([i32; 4], Option<[i32; 4]>)> {
        self.build_frame(ms, &[]);
        let mut out = Vec::new();
        for r in self.batch.runs() {
            for i in r.start..r.end {
                let q = self.batch.quads()[i as usize];
                out.push((round(q.rect), r.clip_rect.map(|c| round(c.rect()))));
            }
        }
        out
    }

    /// The rects, rounded, of the frame at `ms`, each unturned quad cut
    /// to its run's inline clip rect: what shows.
    pub fn shown(&mut self, ms: i64) -> Vec<[i32; 4]> {
        self.clipped(ms)
            .into_iter()
            .map(|(r, c)| match c {
                None => r,
                Some([cx, cy, cw, ch]) => {
                    let [x, y, w, h] = r;
                    let (l, t) = (x.max(cx), y.max(cy));
                    let (rt, b) = ((x + w).min(cx + cw), (y + h).min(cy + ch));
                    [l, t, (rt - l).max(0), (b - t).max(0)]
                }
            })
            .collect()
    }
}

/// An imported skin's loading state, its origin at 0, built at chosen
/// times with the shown gauge's descriptor and the progress.
pub struct Loader {
    pub skin: Skin,
    pub uvs: UvTable,
    pub loading: Loading,
    pub batch: QuadBatch,
    /// The descriptor each frame gives: by default the drawn play's gauge.
    pub gauge: Option<LoadingGauge>,
    /// The chosen chart's play level each frame gives: by default none.
    pub play_level: Option<i32>,
    /// The chosen chart's difficulty and BGA flag each frame gives: by
    /// default none.
    pub difficulty: Option<u8>,
    pub bga: Option<bool>,
    pub percent: i32,
    /// Whether the stored covers are being adjusted: START or SELECT held.
    pub adjusting: bool,
    /// Whether the attempt being loaded is autoplay: by default not.
    pub autoplay: bool,
    /// The turntable angle each frame gives: by default 0.
    pub turntable: i32,
}

impl Loader {
    pub fn new(skin: Skin) -> Self {
        let uvs = skin
            .atlas(Variant::Lo, LIMITS, &|| false)
            .unwrap()
            .into_parts()
            .1;
        let loading = skin.prepare_loading(Side::Left);
        let batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        Self {
            skin,
            uvs,
            loading,
            batch,
            gauge: Some(LoadingGauge::of(0, &GAUGES[0])),
            play_level: None,
            difficulty: None,
            bga: None,
            percent: 0,
            adjusting: false,
            autoplay: false,
            turntable: 0,
        }
    }

    /// The quads of the loading frame at `ms`.
    pub fn frame(&mut self, ms: i64) -> Vec<Quad> {
        self.batch.clear();
        let view = LaneView::NONE;
        let mut covers = readouts_for(view).covers;
        covers.adjusting = self.adjusting;
        let frame = LoadingFrame {
            difficulty: self.difficulty,
            bga: self.bga,
            autoplay: self.autoplay,
            now: TimeUs(ms * MS),
            load: LoadState {
                since: TimeUs(0),
                percent: self.percent,
            },
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            turntable: self.turntable,
            lanes: view,
            covers,
            gauge: self.gauge,
            play_level: self.play_level,
        };
        self.skin
            .build_loading(&mut self.loading, &frame, &self.uvs, &mut self.batch)
            .expect("the loading frame builds");
        self.batch.quads().to_vec()
    }

    /// The rects, rounded, of the loading frame at `ms`.
    pub fn rects(&mut self, ms: i64) -> Vec<[i32; 4]> {
        self.frame(ms).iter().map(|q| round(q.rect)).collect()
    }
}

/// The atlas texel at the middle of `q`'s uv, from its first level.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "test atlases are small"
)]
pub fn quad_texel(atlas: &Atlas, q: &Quad) -> [u8; 4] {
    let side = atlas.side();
    let at = |a: f32, b: f32| ((a + b) / 2.0 * side as f32) as u32;
    texel(atlas, q, at(q.uv[0], q.uv[2]), at(q.uv[1], q.uv[3]))
}

/// The atlas texel at fraction `fu` across and `fv` down `q`'s uv, from
/// its first level.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "test atlases are small"
)]
pub fn texel_at(atlas: &Atlas, q: &Quad, fu: f32, fv: f32) -> [u8; 4] {
    let side = atlas.side();
    let at = |a: f32, b: f32, f: f32| ((a + (b - a) * f) * side as f32) as u32;
    texel(atlas, q, at(q.uv[0], q.uv[2], fu), at(q.uv[1], q.uv[3], fv))
}

/// The texel at column `x`, row `y` of `q`'s page, from its first level.
fn texel(atlas: &Atlas, q: &Quad, x: u32, y: u32) -> [u8; 4] {
    let level = atlas.pages()[usize::from(q.layer) - 1].level(0).unwrap();
    let i = usize::try_from((y * atlas.side() + x) * 4).unwrap();
    level[i..i + 4].try_into().unwrap()
}

/// A long note on model lane 1 from 1 s to 1.3 s in `mode`, with a STOP
/// of `stop` ms at its end.
pub fn long_note(mode: Option<LnMode>, stop: i64) -> Chart {
    let mut b = ChartBuilder::new(150_000).long(
        Lane::new(1).unwrap(),
        TimeUs(1_000 * MS),
        TimeUs(1_300 * MS),
    );
    if stop > 0 {
        b = b.stop(TimeUs(1_300 * MS), TimeUs(stop * MS));
    }
    let mut chart = b.build().unwrap();
    chart.meta.ln_mode = mode;
    chart
}

/// A float rect as whole pixels.
#[expect(clippy::cast_possible_truncation, reason = "test pixels are small")]
pub fn round(r: [f32; 4]) -> [i32; 4] {
    r.map(|v| v.round() as i32)
}
