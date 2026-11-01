//! Drawing an imported skin: its atlas, a play of an empty chart, and
//! frames built at chosen times and play states.

#![allow(dead_code)]

use std::collections::HashMap;

use bms_model::{ChartBuilder, Lane, TimeUs};
use eval::{GaugeState, JudgeEvent, JudgeEventKind, Tally};
use rules::{GaugeValue, Judgment, PerJudgment};
use skin_model::{
    AtlasLimits, Attempt, BpmRange, ChartText, CoverSettings, FrameReadouts, FrameUv, GaugeDesc,
    GaugeLook, LaneView, Layout, PlayContext, Quad, QuadBatch, Side, Skin, SkinFrame, TextImage,
    TextTable, UvTable, Variant,
};

pub const MS: i64 = 1_000;

/// A notes' end past every frame the tests build: timer 143 stays off.
pub const NO_END: TimeUs = TimeUs(3_600_000 * MS);

const GAUGES: [GaugeDesc; 1] = [GaugeDesc {
    start: GaugeValue(20 * GaugeValue::PERCENT),
    max: GaugeValue::FULL,
    look: GaugeLook::Normal,
    border: GaugeValue(80 * GaugeValue::PERCENT),
}];

const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 4_096,
    max_layers: 8,
    reserved_layers: 1,
};

/// The readouts of a frame showing `view`: no tempo, hi-speed or duration,
/// and every cover chosen on, stored at the amount `view` draws (capped
/// at 1 000).
pub fn readouts_for(view: LaneView) -> FrameReadouts {
    FrameReadouts {
        bpm_current_x100: None,
        hispeed_x100: None,
        duration: None,
        covers: CoverSettings {
            cover: view.cover.min(1_000),
            lift: view.lift.min(1_000),
            hidden: view.hidden.min(1_000),
            cover_enabled: true,
            lift_enabled: true,
            hidden_enabled: true,
            adjusting: false,
        },
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
    pub batch: QuadBatch,
    /// The hi-speed and covers each frame shows.
    pub view: LaneView,
    /// The render target's size each frame draws at.
    pub target: [u32; 2],
    /// The readouts each frame gives; `None` gives [`readouts_for`] the
    /// view.
    pub readouts: Option<FrameReadouts>,
}

/// The attempt every drawn import plays: the whole chart, no lead-in.
fn context() -> PlayContext<'static> {
    context_with(None)
}

/// [`context`] with the chart's BPM range `bpm_range`.
fn context_with(bpm_range: Option<BpmRange>) -> PlayContext<'static> {
    PlayContext {
        gauges: &GAUGES,
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: NO_END,
        bpm_range,
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
fn chart(notes: u32) -> bms_model::Chart {
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
        self.play = self.skin.prepare(&chart(notes), &context()).unwrap();
    }

    /// `skin` played over `chart`.
    pub fn over(skin: Skin, chart: &bms_model::Chart) -> Self {
        Self::over_with(skin, chart, None)
    }

    /// `skin` played over `chart` whose BPM range is `bpm_range`.
    pub fn over_with(skin: Skin, chart: &bms_model::Chart, bpm_range: Option<BpmRange>) -> Self {
        let uvs = skin.atlas(Variant::Lo, LIMITS).unwrap().into_parts().1;
        let ctx = context_with(bpm_range);
        let play = skin.prepare(chart, &ctx).unwrap();
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
            batch,
            view: LaneView {
                visible_time: TimeUs(600 * MS),
                cover: 0,
                lift: 0,
                hidden: 0,
            },
            target: [1_920, 1_080],
            readouts: None,
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
        let frame = SkinFrame {
            now: TimeUs(ms * MS),
            display_offset: TimeUs(0),
            tally: &self.tally,
            gauges: &self.gauges,
            gauge: 0,
            effects,
            complete: false,
            target: self.target,
            layout: &Layout::AUTHOR,
            lanes: self.view,
            loading: None,
            playing: true,
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

/// A float rect as whole pixels.
#[expect(clippy::cast_possible_truncation, reason = "test pixels are small")]
pub fn round(r: [f32; 4]) -> [i32; 4] {
    r.map(|v| v.round() as i32)
}
