//! Self-made result skins and their frames, over the selection fixtures'
//! sheet (every texel naming its own place), imported through the
//! production path (`import` or the Lua header's import, then
//! `prepare_result`, `update_result` and `build_result`), and result views
//! built in code.

#![allow(dead_code)]

use std::path::Path;

use bms_model::TimeUs;
use eval::Tally;
use rules::PerJudgment;
use skin_beatoraja::{
    Cancel, Config, Fact, ImportError, ImportFacts, Imported, LuaImported, LuaOutcome, import,
    read_header, read_lua_header,
};
use skin_model::{
    Atlas, BestFigures, BestRate, BestView, LampKey, Layout, Quad, QuadBatch, ResultBpm,
    ResultChart, ResultFields, ResultFrame, ResultLabels, ResultNumber, ResultRate, ResultScore,
    ResultScreen, ResultStatus, ResultTimers, ResultView, ScreenKind, Skin, TimingStats,
    Unavailable, Variant,
};
use tempfile::TempDir;

use super::play::{LIMITS, quad_texel};
use super::selection::{IMAGES, SHEET, digit_of};
use super::{coord, font, png_with, write};

/// A result skin folder holding `s.json` with `body` after the type, size,
/// sources, fonts and the selection fixtures' images: `s.png` and `f.ttf`.
pub fn folder(body: &str) -> TempDir {
    let d = sheet();
    write(d.path(), "s.json", json(body).as_bytes());
    d
}

/// A folder with only the sheet and the font.
pub fn sheet() -> TempDir {
    let d = tempfile::tempdir().unwrap();
    png_with(d.path(), "s.png", SHEET, SHEET, coord);
    font(d.path(), "f.ttf");
    d
}

/// The definition [`folder`] writes.
pub fn json(body: &str) -> String {
    format!(
        r#"{{"type": 7, "w": 1920, "h": 1080,
            "source": [{{"id": "s", "path": "s.png"}}],
            "font": [{{"id": "f", "path": "f.ttf"}}],
            "image": [{IMAGES}], {body}}}"#
    )
}

/// `s.json` of `root` imported into the result slot with the defaults.
pub fn import_result(root: &Path) -> Result<Imported, ImportError> {
    let (mut def, h) = read_header(root, "s.json")?;
    import(&mut def, &h, &Config::defaults(&h), ScreenKind::Result)
}

/// The facts of a result scene showing `view`, the chart's own known.
pub fn facts(view: Option<&ResultView>) -> ImportFacts {
    ImportFacts {
        result: view.map_or(Fact::Unavailable, |v| Fact::Known(ResultFields::of(v))),
        ..ImportFacts::unavailable(ScreenKind::Result)
    }
}

/// Both passes of the Lua result skin `s.luaskin` in `root` against
/// `facts`.
pub fn lua_outcome(root: &Path, facts: &ImportFacts) -> Result<LuaOutcome, ImportError> {
    let cancel = Cancel::new();
    let h = read_lua_header(root, "s.luaskin", &cancel)?;
    h.import(&Config::defaults(h.header()), facts, &cancel)
}

/// [`lua_outcome`], imported.
#[track_caller]
pub fn lua_result(root: &Path, facts: &ImportFacts) -> Result<LuaImported, ImportError> {
    match lua_outcome(root, facts)? {
        LuaOutcome::Imported(i) => Ok(i),
        LuaOutcome::Pending(p) => panic!("{p:?}"),
    }
}

/// The chart's strings the tests pass.
pub fn labels() -> ResultLabels {
    ResultLabels {
        title: "Song".to_owned(),
        subtitle: String::new(),
        artist: "Someone".to_owned(),
        subartist: String::new(),
        genre: String::new(),
    }
}

/// A tally of `ex` EX score and `bp` BP.
pub fn tally(ex: u32, bp: u32) -> Tally {
    let counts = PerJudgment {
        perfect: 400,
        great: 2,
        good: 3,
        bad: 4,
        poor: 5,
    };
    Tally {
        counts,
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
            perfect: 390,
            great: 1,
            good: 2,
            bad: 2,
            poor: 2,
        },
        empty_poor_early: 4,
        empty_poor_late: 2,
    }
}

/// The chart's facts: level 12, every other known.
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
    }
}

/// A known best whose BP difference is `bp_diff`.
pub fn best(bp_diff: ResultNumber) -> BestView {
    BestView::Known(BestFigures {
        ex: 900,
        ex_diff: 100,
        bp: match bp_diff {
            ResultNumber::Shown(d) => ResultNumber::Shown(20 - d),
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

/// A ready view of `ex` EX score, cleared when `cleared`, with `best`.
pub fn ready(ex: u32, cleared: bool, best: BestView) -> ResultView {
    ResultView {
        status: ResultStatus::Ready,
        chart: chart(),
        score: ResultScore {
            tally: tally(ex, 20),
            gauge_permille: 805,
            clear: if cleared {
                LampKey::Normal
            } else {
                LampKey::Failed
            },
            cleared,
            rate: Some(ResultRate {
                rate_bp: 8_333,
                grade: 6,
            }),
            next_rank: 17,
            duration_us: Some(123_456),
        },
        best,
        timing: Some(TimingStats {
            samples: 500,
            mean_x100: -149,
            sd_x100: 1_234,
        }),
        timelines: None,
        gauge: None,
        deviations: None,
        timing_areas: None,
    }
}

/// A view with no result: the chart's facts alone.
pub fn unavailable() -> ResultView {
    ResultView {
        status: ResultStatus::Unavailable(Unavailable::Failed),
        ..ready(1, true, BestView::Unavailable)
    }
}

/// An imported result skin drawn: its atlas, state and batch.
pub struct Res {
    pub skin: Skin,
    pub atlas: Atlas,
    pub uvs: skin_model::UvTable,
    pub res: ResultScreen,
    pub batch: QuadBatch,
    pub timers: ResultTimers,
}

impl Res {
    pub fn new(skin: Skin) -> Self {
        let atlas = skin.atlas(Variant::Lo, LIMITS, &|| false).unwrap();
        let uvs = atlas.uvs().clone();
        let res = skin.prepare_result();
        let batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        Self {
            skin,
            atlas,
            uvs,
            res,
            batch,
            timers: ResultTimers {
                screen: Some(TimeUs(0)),
                fadeout: None,
            },
        }
    }

    pub fn view(&mut self, view: &ResultView) {
        self.skin
            .update_result(&mut self.res, view, &labels())
            .unwrap();
    }

    /// The quads of the frame at `ms`.
    pub fn frame(&mut self, ms: i64) -> Vec<Quad> {
        self.batch.clear();
        let frame = ResultFrame {
            now: TimeUs(ms * 1_000),
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            timers: self.timers,
        };
        self.skin
            .build_result(&mut self.res, &frame, &self.uvs, &mut self.batch)
            .unwrap();
        self.batch.quads().to_vec()
    }

    /// The frame at `ms`: each quad's middle texel (its image) and its top.
    pub fn drawn(&mut self, ms: i64) -> Vec<([u8; 2], f32)> {
        let quads = self.frame(ms);
        quads
            .iter()
            .map(|q| {
                let [x, y, ..] = quad_texel(&self.atlas, q);
                ([x, y], q.rect[1])
            })
            .collect()
    }

    /// The images drawn at `ms` whose top is `top`, digits as their glyph.
    pub fn at(&mut self, ms: i64, top: f32) -> Vec<[u8; 2]> {
        self.drawn(ms)
            .into_iter()
            .filter(|&(_, y)| y == top)
            .map(|(t, _)| t)
            .collect()
    }

    /// The digits drawn at `ms` whose top is `top`.
    pub fn digits(&mut self, ms: i64, top: f32) -> String {
        self.at(ms, top)
            .into_iter()
            .filter_map(digit_of)
            .map(|d| char::from(b'0' + d))
            .collect()
    }
}

/// The destinations of the essential parts a graph fixture holds: the EX
/// score and an image under the clear status.
const GRAPH_ESSENTIALS: &str = r#"{"id": "ex", "dst": [{"x": 0, "y": 100, "w": 4, "h": 8}]},
    {"id": "dot", "op": [90], "dst": [{"x": 0, "y": 300, "w": 8, "h": 8}]}"#;

/// A JSON result skin body for [`folder`]: the EX score's number, `tables`
/// (the graph records, each a member ending in a comma), and the
/// essentials' destinations then `dsts`.
pub fn graph_json(tables: &str, dsts: &str) -> String {
    format!(
        r#""value": [{{"id": "ex", "src": "s", "x": 0, "y": 32, "w": 40, "h": 8, "divx": 10, "digit": 4, "ref": 71}}],
           {tables}
           "destination": [{GRAPH_ESSENTIALS}, {dsts}]"#
    )
}

/// The Lua skin of the same parts: `tables` as Lua statements and `dsts`
/// as destination entries.
pub fn graph_lua(tables: &str, dsts: &str) -> String {
    format!(
        "local skin = {{ type = 7, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         skin.source = {{ {{ id = \"s\", path = \"s.png\" }} }}\n\
         skin.font = {{ {{ id = \"f\", path = \"f.ttf\" }} }}\n\
         skin.image = {{ {{ id = \"dot\", src = \"s\", x = 48, y = 0, w = 8, h = 8 }} }}\n\
         skin.value = {{ {{ id = \"ex\", src = \"s\", x = 0, y = 32, w = 40, h = 8, divx = 10, \
             digit = 4, ref = 71 }} }}\n\
         {tables}\n\
         skin.destination = {{\n\
           {{ id = \"ex\", dst = {{ {{ x = 0, y = 100, w = 4, h = 8 }} }} }},\n\
           {{ id = \"dot\", op = {{ 90 }}, dst = {{ {{ x = 0, y = 300, w = 8, h = 8 }} }} }},\n\
           {dsts} }}\n\
         return skin\n"
    )
}
