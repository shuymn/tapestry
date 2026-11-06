//! The recent timing a play skin reads (docs/stage3.md R3): number 525,
//! the latest judgment's note time minus press in ms, and conditions 1242
//! and 1243, that judgment worse than PERFECT and early or late. Scripted
//! input goes through the evaluator to an imported JSON and Lua skin's
//! drawn digits and images.

mod common;

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use common::play::{Drawn, Loader, MS, context};
use common::{import_body, write};
use eval::{
    Binding, ControlId, DeviceProfile, EvalInput, InputKind, JudgeEvent, NoteFacts,
    PhysicalInputEvent, SourceId, SourceProfile,
};
use skin_beatoraja::{Cancel, Config, read_lua_header};
use skin_model::{Quad, Skin};

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

/// Control `n` set to `value` at `us`.
fn set(us: i64, n: u8, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(us),
        source: SourceId(0),
        kind: InputKind::Control {
            control: ControlId(u32::from(n)),
            value,
        },
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

/// The scripted play: the first tap 30 ms early (GREAT), the second on
/// time (PERFECT), then a press 0.5 ms after the second (an empty POOR
/// against it); the third missed; a press 300 ms before the fourth (an
/// empty POOR against it), the fourth missed; the long note's head 10 ms
/// early (held PERFECT), released 40 ms before its end (PERFECT).
fn inputs() -> Vec<PhysicalInputEvent> {
    vec![
        set(970 * MS, 1, 1),
        set(990 * MS, 1, 0),
        set(2_000 * MS, 1, 1),
        set(2_000 * MS + 300, 1, 0),
        set(2_000 * MS + 500, 1, 1),
        set(2_020 * MS, 1, 0),
        set(5_100 * MS, 1, 1),
        set(5_120 * MS, 1, 0),
        set(5_990 * MS, 2, 1),
        set(6_460 * MS, 2, 0),
    ]
}

/// The evaluator's events for the scripted play under the
/// beatoraja-style preset.
fn evaluated() -> Vec<JudgeEvent> {
    let input = EvalInput {
        events: inputs(),
        device: device(),
        ..EvalInput::default()
    };
    eval::evaluate(&chart(), &input, &rules::beatoraja_style())
        .unwrap()
        .events
}

/// What a frame shows: number 525, `None` when undrawn, and whether the
/// 1242 (early) and 1243 (late) images are drawn.
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

/// The number's 24-glyph strip: digits, padding and a sign per half,
/// the second half for a negative value.
struct Strip {
    /// The uv-table index of the first half's 0.
    zero: usize,
}

impl Strip {
    /// Reads `quads`: the number's cells are 12 wide, the early image 30
    /// and the late 40.
    fn read(&self, d: &Drawn, quads: &[Quad]) -> Shown {
        let width = |q: &Quad| q.rect[2].round() as i32;
        let mut cells: Vec<&Quad> = quads.iter().filter(|q| width(q) == 12).collect();
        cells.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
        let ms = (!cells.is_empty()).then(|| {
            let glyphs: Vec<usize> = cells
                .iter()
                .map(|q| {
                    let i = d
                        .uvs
                        .frames()
                        .iter()
                        .position(|f| f.uv == q.uv && f.layer == q.layer)
                        .expect("a frame of the sheet");
                    i - self.zero
                })
                .collect();
            let (sign, digits) = glyphs.split_first().expect("a sign cell");
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

/// A play of `skin` over the chart, its frames fed the events up to each
/// frame's time.
struct Play {
    d: Drawn,
    strip: Strip,
    events: Vec<JudgeEvent>,
    /// The events already delivered.
    next: usize,
}

impl Play {
    fn new(skin: Skin) -> Self {
        let mut d = Drawn::over(skin, &chart());
        // Before any judgment the number reads 0: its cells, a sign and
        // the first half's 0, name the strip.
        let mut quads = d.frame(0, &[]);
        quads.retain(|q| q.rect[2].round() as i32 == 12);
        quads.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
        let index = |q: &Quad| {
            d.uvs
                .frames()
                .iter()
                .position(|f| f.uv == q.uv && f.layer == q.layer)
                .unwrap()
        };
        assert_eq!(quads.len(), 2);
        let zero = index(&quads[1]);
        assert_eq!(index(&quads[0]), zero + 11);
        Self {
            d,
            strip: Strip { zero },
            events: evaluated(),
            next: 0,
        }
    }

    /// The frame at `ms` with the events since the last frame.
    fn at(&mut self, ms: i64) -> Shown {
        let end = self.next
            + self.events[self.next..]
                .iter()
                .take_while(|e| e.time.0 <= ms * MS)
                .count();
        let quads = self.d.frame(ms, &self.events[self.next..end]);
        self.next = end;
        self.strip.read(&self.d, &quads)
    }

    /// A fresh attempt over the chart, as a retry prepares it, its events
    /// delivered again from the start.
    fn retry(&mut self) {
        self.d.notes = NoteFacts::new(&self.d.chart);
        self.d.play = self.d.skin.prepare(&self.d.chart, &context()).unwrap();
        self.next = 0;
    }
}

/// The scripted play's frames, each just after an event.
fn walk(p: &mut Play) {
    // Nothing judged yet: 0, neither image.
    assert_eq!(p.at(500), shown(0, false, false), "before any");
    // 30 ms early, GREAT: +30, early.
    assert_eq!(p.at(980), shown(30, true, false), "early GREAT");
    // On time, PERFECT: 0, neither, so nothing keeps the GREAT's.
    assert_eq!(p.at(2_000), shown(0, false, false), "PERFECT");
    // An empty POOR 0.5 ms after its note: truncated toward 0, neither.
    assert_eq!(p.at(2_001), shown(0, false, false), "sub-millisecond");
    // A miss at the late BAD edge, 210 ms past its note: late.
    assert_eq!(p.at(3_300), shown(-210, false, true), "miss");
    // An empty POOR 300 ms before its note: early.
    assert_eq!(p.at(5_150), shown(300, true, false), "empty POOR");
    assert_eq!(p.at(5_700), shown(-210, false, true), "second miss");
    // The long note's held head leaves the miss's timing.
    assert_eq!(p.at(6_000), shown(-210, false, true), "held head");
    // Its release 40 ms before the end, farther than the head's 10 ms:
    // +40, PERFECT, neither.
    assert_eq!(p.at(6_470), shown(40, false, false), "release");
}

fn json_skin() -> common::Loaded {
    let loaded = import_body(
        r#""value": [{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 24, "digit": 4,
                      "ref": 525}],
           "image": [{"id": "i", "src": "a", "w": 64, "h": 64}],
           "destination": [{"id": "v", "dst": [{"w": 12, "h": 20}]},
                           {"id": "i", "op": [1242], "dst": [{"y": 100, "w": 30, "h": 10}]},
                           {"id": "i", "op": [1243], "dst": [{"y": 100, "w": 40, "h": 10}]},
                           {"id": "i", "dst": [{"y": 200, "w": 50, "h": 10}]}]"#,
    );
    assert!(
        loaded.report().notes.is_empty(),
        "{:?}",
        loaded.report().notes
    );
    loaded
}

/// The JSON skin's digits and images follow the evaluated play.
#[test]
fn a_json_skin_shows_the_recent_timing() {
    let loaded = json_skin();
    walk(&mut Play::new(loaded.ok().skin.clone()));
}

/// A retry and a rearmed READY read 0 and neither image, until the next
/// judgment.
#[test]
fn a_retry_and_a_rearm_start_at_zero() {
    let loaded = json_skin();
    let mut p = Play::new(loaded.ok().skin.clone());
    assert_eq!(p.at(980), shown(30, true, false));
    p.d.play.rearm();
    assert_eq!(p.at(990), shown(0, false, false), "rearmed");
    assert_eq!(p.at(3_300), shown(-210, false, true), "after the rearm");
    p.retry();
    assert_eq!(p.at(500), shown(0, false, false), "retried");
    assert_eq!(p.at(980), shown(30, true, false), "after the retry");
}

/// While loading, the recent timing is unavailable: the number draws
/// nothing and neither condition holds, while the unconditioned image, 50
/// wide, draws.
#[test]
fn loading_draws_no_timing() {
    let mut l: Loader = json_skin().loading();
    for ms in [0, 100] {
        assert_eq!(l.rects(ms).iter().map(|r| r[2]).collect::<Vec<_>>(), [50]);
    }
}

/// A Lua skin's number callback and draw callback read the same fields.
#[test]
fn a_lua_skin_shows_the_recent_timing() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "s.luaskin",
        b"local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
          if not skin_config then return skin end\n\
          local ms = require(\"main_state\")\n\
          skin.source = { { id = \"a\", path = \"a.png\" }, { id = \"n\", path = \"n.png\" } }\n\
          skin.image = { { id = \"i\", src = \"a\", x = 0, y = 0, w = 8, h = 8 } }\n\
          skin.value = { { id = \"v\", src = \"n\", x = 0, y = 0, w = 120, h = 20, divx = 24, digit = 4,\n\
            value = function() return ms.number(525) end } }\n\
          skin.destination = {\n\
            { id = \"v\", dst = { { w = 12, h = 20 } } },\n\
            { id = \"i\", op = { 1242 }, dst = { { y = 100, w = 30, h = 10 } } },\n\
            { id = \"i\", draw = function() return ms.option(1243) end,\n\
              dst = { { y = 100, w = 40, h = 10 } } } }\n\
          return skin\n",
    );
    common::png(d.path(), "a.png", 64, 64);
    common::png(d.path(), "n.png", 120, 20);
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let i = common::lua_play(&h, &Config::default(), &cancel).unwrap();
    assert!(
        i.imported.report.notes.is_empty(),
        "{:?}",
        i.imported.report.notes
    );
    walk(&mut Play::new(i.imported.skin));
}
