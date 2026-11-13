//! Result skins for the result tests: elements over the selection tests'
//! sheet, expressions over the result schema, and result views built in
//! code.

#![allow(dead_code)]

use bms_model::TimeUs;
use eval::Tally;
use rules::PerJudgment;
use skin_model::{
    BestFigures, BestRate, BestView, Blend, ElementDef, LampKey, Layout, LoadError, NumberDef,
    Place, Program, QuadBatch, RESULT, ResultBpm, ResultChart, ResultDefs, ResultFrame,
    ResultLabels, ResultNumber, ResultRate, ResultScore, ResultScreen, ResultStatus, ResultTimers,
    ResultView, Scope, Skin, Source, Timelines, TimerRef, TimingStats, Ty, UvTable, WidgetDefs,
    compile, result_timer,
};

use crate::selection_support::{Drawn, digits, frame_of, rect, skin_def, uvs};

pub fn rbool(src: &str) -> Program {
    compile(src, &Scope::new(&RESULT), Ty::Bool).unwrap()
}

pub fn rint(src: &str) -> Program {
    compile(src, &Scope::new(&RESULT), Ty::Int).unwrap()
}

/// An element drawing `source` over `r` on `screen`, drawn while
/// `visible` holds.
pub fn relement(source: Source, r: [i32; 4], visible: Option<&str>) -> ElementDef {
    ElementDef {
        source,
        track: rect(r),
        timer: TimerRef::Builtin(result_timer::SCREEN),
        visible: visible.map(rbool),
        blend: Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    }
}

/// A number of `cells` ten-glyph cells showing `value`.
pub fn number(value: &str, cells: u8) -> NumberDef {
    NumberDef {
        digits: digits(cells),
        value: rint(value),
    }
}

/// A result skin of `elements` with `widgets`.
pub fn rload(elements: Vec<ElementDef>, widgets: WidgetDefs) -> Result<Skin, LoadError> {
    Skin::new_result(skin_def(elements, widgets), ResultDefs::default())
}

/// The chart's strings the tests pass.
pub fn labels() -> ResultLabels {
    ResultLabels {
        title: "Song".to_owned(),
        subtitle: "[X]".to_owned(),
        artist: "Someone".to_owned(),
        subartist: String::new(),
        genre: "Genre".to_owned(),
    }
}

/// A tally of `ex` EX score, `bp` BP and `perfect` PERFECTs.
pub fn tally(ex: u32, bp: u32, perfect: u32) -> Tally {
    Tally {
        counts: PerJudgment {
            perfect,
            great: 2,
            good: 3,
            bad: 4,
            poor: 5,
        },
        ex_score: ex,
        bp,
        combo: 0,
        max_combo: 77,
        empty_poor: 6,
        early: PerJudgment {
            perfect: 10,
            great: 1,
            good: 1,
            bad: 2,
            poor: 3,
        },
        late: PerJudgment {
            perfect: perfect.saturating_sub(10),
            great: 1,
            good: 2,
            bad: 2,
            poor: 2,
        },
        empty_poor_early: 4,
        empty_poor_late: 2,
    }
}

/// The chart's facts of the tests: every one known.
pub fn chart() -> ResultChart {
    ResultChart {
        level: Some(12),
        total: Some(300),
        bpm: Some(ResultBpm {
            min_x100: 15_000,
            max_x100: 18_050,
            changes: true,
        }),
        difficulty: Some(4),
        length: Some(TimeUs(123_456_789)),
        notes: Some(600),
        judge_class: None,
        images: skin_model::ImageFacts {
            stage: true,
            back: false,
        },
    }
}

/// A known best whose BP difference is `bp_diff`.
pub fn best(bp_diff: ResultNumber) -> BestView {
    BestView::Known(BestFigures {
        ex: 900,
        ex_diff: 100,
        bp: match bp_diff {
            ResultNumber::Shown(_) => ResultNumber::Shown(23),
            ResultNumber::Hidden => ResultNumber::Hidden,
        },
        bp_diff,
        clear: LampKey::Hard,
        ex_updated: true,
        bp_updated: matches!(bp_diff, ResultNumber::Shown(d) if d < 0),
        rate: Some(BestRate {
            rate_bp: 7_500,
            grade: 5,
            updated: true,
        }),
    })
}

/// A ready view of `ex` EX score with `best` and timelines of `bins`.
pub fn ready(ex: u32, best: BestView, bins: usize) -> ResultView {
    ResultView {
        status: ResultStatus::Ready,
        chart: chart(),
        score: ResultScore {
            tally: tally(ex, 20, 400),
            gauge_permille: 805,
            clear: LampKey::Normal,
            cleared: true,
            rate: Some(ResultRate {
                rate_bp: 8_333,
                grade: 6,
            }),
            next_rank: 17,
            duration_us: Some(12_345),
        },
        best,
        timing: Some(TimingStats {
            samples: 500,
            mean_x100: -375,
            sd_x100: 1_234,
        }),
        timelines: Some(
            Timelines::new(
                TimeUs(1_000_000),
                (0..bins).map(|i| [u32::try_from(i).unwrap(); 9]).collect(),
                (0..bins).map(|i| u32::try_from(10 * i).unwrap()).collect(),
                None,
            )
            .unwrap(),
        ),
        gauge: None,
        deviations: None,
        timing_areas: None,
    }
}

/// A ready view whose score, best, timing and timelines all differ from
/// [`ready`]'s: EX 5 678, not cleared, a best of EX 444 and no BP, timing
/// SD 4 321, three bins with the last uncovered.
pub fn other() -> ResultView {
    ResultView {
        status: ResultStatus::Ready,
        chart: ResultChart {
            notes: Some(950),
            ..chart()
        },
        score: ResultScore {
            tally: tally(5_678, 3, 900),
            gauge_permille: 120,
            clear: LampKey::Failed,
            cleared: false,
            rate: Some(ResultRate {
                rate_bp: 2_988,
                grade: 1,
            }),
            next_rank: 3,
            duration_us: Some(1_000_000),
        },
        best: BestView::Known(BestFigures {
            ex: 444,
            ex_diff: 5_234,
            bp: ResultNumber::Hidden,
            bp_diff: ResultNumber::Hidden,
            clear: LampKey::NoPlay,
            ex_updated: true,
            bp_updated: true,
            rate: None,
        }),
        timing: Some(TimingStats {
            samples: 7,
            mean_x100: 42,
            sd_x100: 4_321,
        }),
        timelines: Some(
            Timelines::new(
                TimeUs(2_000_000),
                vec![[9; 9], [8; 9], [0; 9]],
                vec![90, 80, 7],
                Some(2),
            )
            .unwrap(),
        ),
        gauge: None,
        deviations: None,
        timing_areas: None,
    }
}

/// A result skin with its state, uv table and batch.
pub struct RFx {
    pub skin: Skin,
    pub res: ResultScreen,
    pub uvs: UvTable,
    pub batch: QuadBatch,
    pub target: [u32; 2],
    pub layout: Layout,
    pub timers: ResultTimers,
    /// The application's readouts each frame is built with.
    pub app: skin_model::AppReadouts,
}

impl RFx {
    pub fn new(skin: Skin) -> Self {
        let res = skin.prepare_result();
        let batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        Self {
            uvs: uvs(&skin),
            skin,
            res,
            batch,
            target: [1_920, 1_080],
            layout: Layout::AUTHOR,
            timers: ResultTimers {
                screen: Some(TimeUs(0)),
                fadeout: None,
            },
            app: skin_model::AppReadouts::LAUNCH,
        }
    }

    pub fn view(&mut self, view: &ResultView) {
        self.skin
            .update_result(&mut self.res, view, &labels())
            .unwrap();
    }

    /// Builds the frame at `ms`.
    pub fn frame(&mut self, ms: i64) {
        self.batch.clear();
        let frame = ResultFrame {
            now: TimeUs(ms * 1_000),
            target: self.target,
            layout: &self.layout,
            timers: self.timers,
            app: self.app,
        };
        self.skin
            .build_result(&mut self.res, &frame, &self.uvs, &mut self.batch)
            .unwrap();
    }

    /// The quads drawn: image, frame and rect.
    pub fn drawn(&self) -> Vec<Drawn> {
        self.batch
            .quads()
            .iter()
            .map(|q| (frame_of(&self.uvs, q), q.rect))
            .collect()
    }

    /// The image and frame of each quad drawn whose top is at `y`, in draw
    /// order: one element's quads, as the tests place each element at its
    /// own `y`.
    pub fn at_y(&self, y: f32) -> Vec<(u32, u16)> {
        self.drawn()
            .into_iter()
            .filter(|(_, r)| r[1] == y)
            .filter_map(|(f, _)| f)
            .collect()
    }
}
