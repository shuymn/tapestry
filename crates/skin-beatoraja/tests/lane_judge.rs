//! The lanes' visual states a play skin selects by (docs/stage3.md R3):
//! image-index properties 500 (the scratch) to 507 (key 7), the source's
//! per-lane judge value. Scripted input goes through the evaluator to an
//! imported JSON and Lua skin's image groups, each selecting its frame by
//! one lane's value.

mod common;

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use common::play::{Drawn, Loader, MS, context};
use common::{import_body, write};
use eval::{
    Binding, ControlId, DeviceProfile, EvalInput, InputFacts, InputKind, JudgeEvent, NoteFacts,
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

/// Control `n` set to `value` at `ms`.
fn set(ms: i64, n: u8, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(ms * MS),
        source: SourceId(0),
        kind: InputKind::Control {
            control: ControlId(u32::from(n)),
            value,
        },
    }
}

/// The scratch's tap at 1 s; key 1's taps at 1, 2 and 3 s; key 2's long
/// note from 4 to 4.5 s; key 3's taps at 5 and 5.5 s.
fn chart() -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(0), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(2_000 * MS))
        .tap(lane(1), TimeUs(3_000 * MS))
        .long(lane(2), TimeUs(4_000 * MS), TimeUs(4_500 * MS))
        .tap(lane(3), TimeUs(5_000 * MS))
        .tap(lane(3), TimeUs(5_500 * MS))
        .build()
        .unwrap()
}

/// The scripted play, each press released 5 ms later unless said.
fn inputs() -> Vec<PhysicalInputEvent> {
    let mut out = Vec::new();
    let mut tap = |ms: i64, n: u8| {
        out.push(set(ms, n, 1));
        out.push(set(ms + 5, n, 0));
    };
    // Key 1 30 ms early: GREAT, early.
    tap(970, 1);
    // The scratch on time: PERFECT.
    tap(1_000, 0);
    // Key 1 40 ms late: GREAT, late.
    tap(2_040, 1);
    // 100 ms after that note: an empty POOR against it, late.
    tap(2_100, 1);
    // 300 ms before the third: an empty POOR, early; it is then missed.
    tap(2_700, 1);
    // 400 ms after the third, the lane's last: a press matching nothing.
    tap(3_400, 1);
    let mut out2 = out;
    // Key 2's head 10 ms early, held; released 140 ms before its end:
    // GOOD under the chart's judge rank, the release farther, early.
    out2.push(set(3_990, 2, 1));
    out2.push(set(4_360, 2, 0));
    // Key 3 100 ms late, GOOD; 150 ms early, BAD.
    out2.push(set(5_100, 3, 1));
    out2.push(set(5_105, 3, 0));
    out2.push(set(5_350, 3, 1));
    out2.push(set(5_355, 3, 0));
    out2.sort_by_key(|e| e.received);
    out2
}

/// The evaluation of `inputs` over `chart` under the beatoraja-style
/// preset.
fn evaluate(chart: &Chart, inputs: &[PhysicalInputEvent]) -> eval::Evaluation {
    let input = EvalInput {
        events: inputs.to_vec(),
        device: device(),
        ..EvalInput::default()
    };
    eval::evaluate(chart, &input, &rules::beatoraja_style()).unwrap()
}

/// A play of `skin` over the chart, its frames fed the events and the lane
/// input up to each frame's time.
struct Play {
    d: Drawn,
    /// Per lane, the uv-table index of its group's first frame.
    zero: [usize; 8],
    events: Vec<JudgeEvent>,
    inputs: Vec<PhysicalInputEvent>,
    /// The events and inputs already delivered.
    next: (usize, usize),
}

impl Play {
    /// The scripted play of [`inputs`] over [`chart`].
    fn new(skin: Skin) -> Self {
        Self::of(skin, &chart(), inputs())
    }

    /// The play of `inputs` over `chart`.
    fn of(skin: Skin, chart: &Chart, inputs: Vec<PhysicalInputEvent>) -> Self {
        let events = evaluate(chart, &inputs).events;
        let mut d = Drawn::over(skin, chart);
        let quads = d.frame(0, &[]);
        let zero = std::array::from_fn(|l| {
            let q = Self::of_lane(&quads, l);
            assert_eq!(q.len(), 1, "lane {l}");
            Self::index(&d, q[0])
        });
        Self {
            d,
            zero,
            events,
            inputs,
            next: (0, 0),
        }
    }

    /// The group quads of lane `l`: 10 wide, at `x = 100 · l`.
    fn of_lane(quads: &[Quad], l: usize) -> Vec<&Quad> {
        quads
            .iter()
            .filter(|q| q.rect[2].round() as i32 == 10 && q.rect[0].round() as usize == 100 * l)
            .collect()
    }

    fn index(d: &Drawn, q: &Quad) -> usize {
        d.uvs
            .frames()
            .iter()
            .position(|f| f.uv == q.uv && f.layer == q.layer)
            .expect("a frame of the sheet")
    }

    /// Each lane's selected frame in the frame at `ms`, with the events
    /// and the lane edges up to it; `None` for a lane drawing nothing.
    fn at(&mut self, ms: i64) -> Vec<Option<usize>> {
        let us = ms * MS;
        let (e0, i0) = self.next;
        let e1 = e0
            + self.events[e0..]
                .iter()
                .take_while(|e| e.time.0 <= us)
                .count();
        let i1 = i0
            + self.inputs[i0..]
                .iter()
                .take_while(|e| e.received.0 <= us)
                .count();
        for e in &self.inputs[i0..i1] {
            let InputKind::Control { control, value } = e.kind else {
                unreachable!()
            };
            let l = lane(u8::try_from(control.0).unwrap());
            if value != 0 {
                self.d.input.press(l, e.received);
            } else {
                self.d.input.release(l, e.received);
            }
        }
        let quads = self.d.frame(ms, &self.events[e0..e1]);
        self.next = (e1, i1);
        (0..8)
            .map(|l| {
                let q = Self::of_lane(&quads, l);
                assert!(q.len() <= 1, "lane {l}");
                q.first().map(|q| Self::index(&self.d, q) - self.zero[l])
            })
            .collect()
    }

    /// A fresh attempt over the chart, as a retry prepares it, its events
    /// and input delivered again from the start.
    fn retry(&mut self) {
        self.d.notes = NoteFacts::new(&self.d.chart);
        self.d.input = InputFacts::NONE;
        self.d.play = self.d.skin.prepare(&self.d.chart, &context()).unwrap();
        self.next = (0, 0);
    }
}

/// Lane values, every lane drawn.
fn lanes(v: [usize; 8]) -> Vec<Option<usize>> {
    v.into_iter().map(Some).collect()
}

/// The scripted play's frames, each just after its events.
fn walk(p: &mut Play) {
    assert_eq!(p.at(500), lanes([0; 8]), "before any");
    assert_eq!(p.at(980), lanes([0, 2, 0, 0, 0, 0, 0, 0]), "early GREAT");
    assert_eq!(p.at(1_000), lanes([1, 2, 0, 0, 0, 0, 0, 0]), "PERFECT");
    assert_eq!(p.at(2_040), lanes([1, 3, 0, 0, 0, 0, 0, 0]), "late GREAT");
    assert_eq!(p.at(2_100), lanes([1, 11, 0, 0, 0, 0, 0, 0]), "late empty");
    assert_eq!(p.at(2_700), lanes([1, 10, 0, 0, 0, 0, 0, 0]), "early empty");
    // The third note's miss, at its late BAD edge, leaves it.
    assert_eq!(p.at(3_300), lanes([1, 10, 0, 0, 0, 0, 0, 0]), "miss");
    assert_eq!(p.at(3_400), lanes([1, 0, 0, 0, 0, 0, 0, 0]), "unmatched");
    assert_eq!(p.at(3_995), lanes([1, 0, 8, 0, 0, 0, 0, 0]), "held");
    assert_eq!(p.at(4_370), lanes([1, 0, 4, 0, 0, 0, 0, 0]), "released");
    assert_eq!(p.at(5_100), lanes([1, 0, 4, 5, 0, 0, 0, 0]), "late GOOD");
    assert_eq!(p.at(5_350), lanes([1, 0, 4, 6, 0, 0, 0, 0]), "early BAD");
}

/// Eight image groups of twelve 10-wide frames, `ref` 500 to 507, lane
/// `n`'s at `x = 100 · n`, and a 50-wide image on no condition.
fn json_skin() -> common::Loaded {
    let images: Vec<String> = (0..8)
        .map(|n| {
            format!(
                r#"{{"id": "g{n}", "src": "n", "w": 120, "h": 20, "divx": 12, "len": 12,
                     "ref": {}}}"#,
                500 + n
            )
        })
        .chain([r#"{"id": "i", "src": "a", "w": 64, "h": 64}"#.to_owned()])
        .collect();
    let dsts: Vec<String> = (0..8)
        .map(|n| {
            format!(
                r#"{{"id": "g{n}", "dst": [{{"x": {}, "w": 10, "h": 10}}]}}"#,
                100 * n
            )
        })
        .chain([r#"{"id": "i", "dst": [{"y": 200, "w": 50, "h": 10}]}"#.to_owned()])
        .collect();
    let loaded = import_body(&format!(
        r#""image": [{}], "destination": [{}]"#,
        images.join(","),
        dsts.join(",")
    ));
    assert!(
        loaded.report().notes.is_empty(),
        "{:?}",
        loaded.report().notes
    );
    loaded
}

/// The JSON skin's groups follow the evaluated play, lane by lane.
#[test]
fn a_json_skin_shows_each_lane() {
    walk(&mut Play::new(json_skin().ok().skin.clone()));
}

/// An empty POOR pressed at the exact time of its already judged note
/// reads early (10), the side its count takes, not late (11).
#[test]
fn an_empty_poor_on_its_note_reads_early() {
    let c = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    // A GREAT 30 ms early, then a press at the note's exact time.
    let ins = vec![set(970, 1, 1), set(975, 1, 0), set(1_000, 1, 1)];
    let tally = evaluate(&c, &ins).tally;
    assert_eq!(
        (tally.empty_poor_early, tally.empty_poor_late),
        (1, 0),
        "the count's side"
    );
    let mut p = Play::of(json_skin().ok().skin.clone(), &c, ins);
    assert_eq!(p.at(980), lanes([0, 2, 0, 0, 0, 0, 0, 0]), "early GREAT");
    assert_eq!(p.at(1_000), lanes([0, 10, 0, 0, 0, 0, 0, 0]), "on time");
}

/// A rearmed READY and a retry read 0 on every lane, until the next
/// judgment.
#[test]
fn a_retry_and_a_rearm_start_at_zero() {
    let mut p = Play::new(json_skin().ok().skin.clone());
    assert_eq!(p.at(1_000), lanes([1, 2, 0, 0, 0, 0, 0, 0]));
    p.d.play.rearm();
    assert_eq!(p.at(1_010), lanes([0; 8]), "rearmed");
    assert_eq!(
        p.at(2_040),
        lanes([0, 3, 0, 0, 0, 0, 0, 0]),
        "after the rearm"
    );
    p.retry();
    assert_eq!(p.at(500), lanes([0; 8]), "retried");
    assert_eq!(
        p.at(980),
        lanes([0, 2, 0, 0, 0, 0, 0, 0]),
        "after the retry"
    );
}

/// While loading, the lanes' states are unavailable: no group draws,
/// while the unconditioned image, 50 wide, does.
#[test]
fn loading_draws_no_lane_state() {
    let mut l: Loader = json_skin().loading();
    for ms in [0, 100] {
        assert_eq!(l.rects(ms).iter().map(|r| r[2]).collect::<Vec<_>>(), [50]);
    }
}

/// A Lua skin's image groups read the same fields.
#[test]
fn a_lua_skin_shows_each_lane() {
    let d = tempfile::tempdir().unwrap();
    let images: String = (0..8)
        .map(|n| {
            format!(
                "{{ id = \"g{n}\", src = \"n\", x = 0, y = 0, w = 120, h = 20, divx = 12, \
                 len = 12, ref = {} }},\n",
                500 + n
            )
        })
        .collect();
    let dsts: String = (0..8)
        .map(|n| {
            format!(
                "{{ id = \"g{n}\", dst = {{ {{ x = {}, w = 10, h = 10 }} }} }},\n",
                100 * n
            )
        })
        .collect();
    let lua = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         skin.source = {{ {{ id = \"n\", path = \"n.png\" }} }}\n\
         skin.image = {{ {images} }}\n\
         skin.destination = {{ {dsts} }}\n\
         return skin\n"
    );
    write(d.path(), "s.luaskin", lua.as_bytes());
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
