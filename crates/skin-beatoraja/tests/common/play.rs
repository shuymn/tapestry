//! Drawing an imported skin: its atlas, a play of an empty chart, and
//! frames built at chosen times and play states.

#![allow(dead_code)]

use bms_model::{ChartBuilder, Lane, TimeUs};
use eval::{GaugeState, JudgeEvent, JudgeEventKind, Tally};
use rules::{GaugeValue, Judgment, PerJudgment};
use skin_model::{
    AtlasLimits, Attempt, ChartText, FrameUv, GaugeDesc, GaugeLook, LaneView, Layout, PlayContext,
    Quad, QuadBatch, Side, Skin, SkinFrame, TextImage, TextTable, UvTable, Variant,
};

pub const MS: i64 = 1_000;

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

    /// `skin` played over `chart`.
    pub fn over(skin: Skin, chart: &bms_model::Chart) -> Self {
        let uvs = skin.atlas(Variant::Lo, LIMITS).unwrap().into_parts().1;
        let ctx = PlayContext {
            gauges: &GAUGES,
            attempt: Attempt::Full { lead_in: TimeUs(0) },
            empty_poor_breaks_combo: false,
            scratch: Side::Left,
        };
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
            },
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
        self.batch.clear();
        let frame = SkinFrame {
            now: TimeUs(ms * MS),
            display_offset: TimeUs(0),
            tally: &self.tally,
            gauges: &self.gauges,
            gauge: 0,
            effects,
            complete: false,
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            lanes: self.view,
        };
        self.skin
            .build(&mut self.play, &frame, &self.uvs, &mut self.batch);
        self.batch.quads().to_vec()
    }

    /// The rects, rounded, of the frame at `ms`.
    pub fn rects(&mut self, ms: i64) -> Vec<[i32; 4]> {
        self.frame(ms, &[]).iter().map(|q| round(q.rect)).collect()
    }
}

/// A float rect as whole pixels.
#[expect(clippy::cast_possible_truncation, reason = "test pixels are small")]
pub fn round(r: [f32; 4]) -> [i32; 4] {
    r.map(|v| v.round() as i32)
}
