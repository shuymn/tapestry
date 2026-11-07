//! A result skin's judge graphs through the importer (docs/stage3.md §3,
//! Result; S10-09h-2): JSON and Lua read the same fields into the same
//! frames, type 0 and unknown types drop with a note (C22); a shared id
//! resolves as the source's lookup resolves it (C23); a `filter` keeps
//! the graph with a note (C26); and the destination's stretch fits the
//! whole graph.

mod common;

use bms_model::TimeUs;
use common::result::{Res, folder, import_result, lua_result, ready, sheet};
use skin_beatoraja::{ImportFacts, NoteKind, Object, Report, Severity};
use skin_model::{BestView, Filter, Quad, ResultView, ScreenKind, Timelines};

/// The essential parts every fixture holds: the EX score and an image
/// under the clear status.
const ESSENTIALS_JSON: &str = r#"{"id": "ex", "dst": [{"x": 0, "y": 100, "w": 4, "h": 8}]},
    {"id": "dot", "op": [90], "dst": [{"x": 0, "y": 300, "w": 8, "h": 8}]}"#;

/// A JSON result skin with `graphs` as its judge graph records, `more`
/// tables, and `dsts` after the essentials' destinations.
fn json_skin(graphs: &str, more: &str, dsts: &str) -> String {
    format!(
        r#""value": [{{"id": "ex", "src": "s", "x": 0, "y": 32, "w": 40, "h": 8, "divx": 10, "digit": 4, "ref": 71}}],
           "judgegraph": [{graphs}], {more}
           "destination": [{ESSENTIALS_JSON}, {dsts}]"#
    )
}

/// The Lua skin of the same parts.
fn lua_skin(graphs: &str, dsts: &str) -> String {
    format!(
        "local skin = {{ type = 7, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         skin.source = {{ {{ id = \"s\", path = \"s.png\" }} }}\n\
         skin.font = {{ {{ id = \"f\", path = \"f.ttf\" }} }}\n\
         skin.image = {{ {{ id = \"dot\", src = \"s\", x = 48, y = 0, w = 8, h = 8 }} }}\n\
         skin.value = {{ {{ id = \"ex\", src = \"s\", x = 0, y = 32, w = 40, h = 8, divx = 10, \
             digit = 4, ref = 71 }} }}\n\
         skin.judgegraph = {{ {graphs} }}\n\
         skin.destination = {{\n\
           {{ id = \"ex\", dst = {{ {{ x = 0, y = 100, w = 4, h = 8 }} }} }},\n\
           {{ id = \"dot\", op = {{ 90 }}, dst = {{ {{ x = 0, y = 300, w = 8, h = 8 }} }} }},\n\
           {dsts} }}\n\
         return skin\n"
    )
}

/// A ready view whose timelines are 70 one-second bins, bin `i` holding
/// `i % 7` of each channel and two notes more.
fn view() -> ResultView {
    let bins: Vec<[u32; 9]> = (0..70).map(|i| [i % 7; 9]).collect();
    let notes = bins.iter().map(|b| b.iter().sum::<u32>() + 2).collect();
    ResultView {
        timelines: Some(Timelines::new(TimeUs(1_000_000), bins, notes, None).unwrap()),
        ..ready(1_000, true, BestView::Unavailable)
    }
}

/// The dropped and ignored notes of a report.
fn notes(r: &Report) -> Vec<(Severity, NoteKind)> {
    let mut n: Vec<_> = r
        .notes
        .iter()
        .filter(|n| matches!(n.severity, Severity::Dropped | Severity::Ignored))
        .map(|n| (n.severity, n.kind))
        .collect();
    n.sort_by_key(|n| format!("{n:?}"));
    n
}

/// The quads of the frame at `ms` drawn on a chip grid and on the
/// graphs' white image.
fn graph_quads(r: &mut Res, ms: i64) -> (Vec<Quad>, Vec<Quad>) {
    let quads = r.frame(ms);
    let chips = quads
        .iter()
        .filter(|q| q.filter == Filter::NearestBase)
        .copied()
        .collect();
    let back = quads
        .iter()
        .filter(|q| q.filter == Filter::Nearest && q.rect[0] >= 50.0)
        .copied()
        .collect();
    (chips, back)
}

/// The judge graph records the parity fixtures hold, as JSON and as Lua:
/// type 1 with every field set, type 2 with the defaults, type 0 and type
/// 3.
const GRAPHS_JSON: &str = r#"{"id": "g1", "type": 1, "backTexOff": 0, "delay": 300, "orderReverse": 1, "noGap": 1, "noGapX": 0},
    {"id": "g2", "type": 2},
    {"id": "g0", "type": 0},
    {"id": "g3", "type": 3}"#;
const GRAPHS_LUA: &str = "{ id = \"g1\", type = 1, backTexOff = 0, delay = 300, orderReverse = 1, \
                          noGap = 1, noGapX = 0 }, { id = \"g2\", type = 2 }, \
                          { id = \"g0\", type = 0 }, { id = \"g3\", type = 3 }";
const DSTS_JSON: &str = r#"{"id": "g1", "dst": [{"x": 100, "y": 200, "w": 520, "h": 82}]},
    {"id": "g2", "dst": [{"x": 100, "y": 400, "w": 520, "h": 82, "a": 200}]},
    {"id": "g0", "dst": [{"x": 100, "y": 600, "w": 520, "h": 82}]},
    {"id": "g3", "dst": [{"x": 100, "y": 700, "w": 520, "h": 82}]}"#;
const DSTS_LUA: &str = "{ id = \"g1\", dst = { { x = 100, y = 200, w = 520, h = 82 } } },\n\
     { id = \"g2\", dst = { { x = 100, y = 400, w = 520, h = 82, a = 200 } } },\n\
     { id = \"g0\", dst = { { x = 100, y = 600, w = 520, h = 82 } } },\n\
     { id = \"g3\", dst = { { x = 100, y = 700, w = 520, h = 82 } } }";

/// C22: a JSON and a Lua skin declaring the same judge graphs draw the
/// same frames at every reveal step, each graph its background and chips
/// on a chip grid; type 0 and type 3 drop with a note on both paths.
/// The defaults hold: the type-2 graph reveals over 500 ms, the type-1
/// one over its own 300 ms.
#[test]
fn json_and_lua_judge_graphs_draw_the_same_frames() {
    let j = folder(&json_skin(GRAPHS_JSON, "", DSTS_JSON));
    let json = import_result(j.path()).unwrap_or_else(|e| panic!("{e:?}"));
    let l = sheet();
    common::write(
        l.path(),
        "s.luaskin",
        lua_skin(GRAPHS_LUA, DSTS_LUA).as_bytes(),
    );
    let lua = lua_result(l.path(), &ImportFacts::unavailable(ScreenKind::Result)).unwrap();
    let type_note = (
        Severity::Dropped,
        NoteKind::UnsupportedField { field: "type" },
    );
    for report in [&json.report, &lua.imported.report] {
        let n = notes(report);
        assert_eq!(n.iter().filter(|&&n| n == type_note).count(), 2, "{n:?}");
        assert!(!n.iter().any(|(_, k)| matches!(
            k,
            NoteKind::Unsupported {
                object: Object::JudgeGraph
            }
        )));
    }
    let mut a = Res::new(json.skin);
    let mut b = Res::new(lua.imported.skin);
    a.view(&view());
    b.view(&view());
    for ms in [0, 150, 300, 450, 600] {
        assert_eq!(a.frame(ms), b.frame(ms), "{ms} ms");
    }
    // Two graphs' backgrounds; by 150 ms the type-1 graph reveals half its
    // bins, the type-2 graph under a third; all of both by 500 ms.
    let top = |y: f32| 1_080.0 - y - 82.0;
    let right = |q: &[Quad], y: f32| {
        q.iter()
            .filter(|q| q.rect[1] >= top(y) && q.rect[1] < top(y) + 82.0)
            .map(|q| q.rect[0] + q.rect[2])
            .fold(0.0_f32, f32::max)
    };
    let (chips, back) = graph_quads(&mut a, 150);
    assert!(!back.is_empty());
    assert!((right(&chips, 200.0) - 360.0).abs() <= 520.0 / 70.0);
    assert!((right(&chips, 400.0) - 256.0).abs() <= 520.0 / 70.0);
    let (chips, _) = graph_quads(&mut a, 500);
    assert_eq!(right(&chips, 200.0), 620.0);
    assert_eq!(right(&chips, 400.0), 620.0);
    // Its alpha 200 reaches the type-2 graph's quads.
    assert!(
        chips
            .iter()
            .filter(|q| q.rect[1] >= top(400.0) && q.rect[1] < top(400.0) + 82.0)
            .all(|q| (q.color[3] - 200.0 / 255.0).abs() < 1e-3)
    );
}

/// C23: a shared id resolves as the source's lookup does. An id in
/// `image` and `judgegraph` is the image (an earlier table returns); one
/// in `judgegraph`, `timingdistributiongraph` and `bpmgraph` is the
/// `bpmgraph` (the first later table); one in `judgegraph` and the
/// `gauge` alone is the gauge; one in `judgegraph` alone stands.
#[test]
fn a_shared_id_resolves_as_the_source_looks_it_up() {
    let dst = |id: &str| {
        format!(r#"{{"id": "{id}", "dst": [{{"x": 100, "y": 200, "w": 520, "h": 82}}]}}"#)
    };
    let graph = |id: &str| format!(r#"{{"id": "{id}", "type": 1}}"#);
    let cases: [(&str, &str, Option<Object>); 4] = [
        ("b0", "", None),
        (
            "x",
            r#""bpmgraph": [{"id": "x"}], "timingdistributiongraph": [{"id": "x"}],"#,
            Some(Object::BpmGraph),
        ),
        (
            "x",
            r#""gauge": {"id": "x", "nodes": ["b0"]},"#,
            Some(Object::Gauge),
        ),
        ("x", "", Some(Object::JudgeGraph)),
    ];
    for (id, more, want) in cases {
        let d = folder(&json_skin(&graph(id), more, &dst(id)));
        let imported = import_result(d.path()).unwrap_or_else(|e| panic!("{e:?}"));
        let unsupported: Vec<Object> = imported
            .report
            .notes
            .iter()
            .filter_map(|n| match n.kind {
                NoteKind::Unsupported { object } => Some(object),
                _ => None,
            })
            .collect();
        let mut r = Res::new(imported.skin);
        r.view(&view());
        let (chips, _) = graph_quads(&mut r, 1_000);
        match want {
            // The image: drawn, no graph.
            None => {
                assert!(chips.is_empty());
                assert!(unsupported.is_empty(), "{unsupported:?}");
                assert_eq!(
                    r.frame(1_000).iter().filter(|q| q.rect[2] == 520.0).count(),
                    1
                );
            }
            Some(Object::JudgeGraph) => {
                assert!(!chips.is_empty());
                assert!(unsupported.is_empty(), "{unsupported:?}");
            }
            Some(o) => {
                assert!(chips.is_empty(), "{o:?}");
                assert_eq!(unsupported, [o]);
            }
        }
    }
}

/// C26: a destination `filter` keeps the graph, drawn as without it, with
/// a note.
#[test]
fn a_filter_keeps_the_graph_with_a_note() {
    let dst = |filter: i32| {
        format!(
            r#"{{"id": "g", "filter": {filter}, "dst": [{{"x": 100, "y": 200, "w": 520, "h": 82}}]}}"#
        )
    };
    let frame = |filter| {
        let d = folder(&json_skin(r#"{"id": "g", "type": 2}"#, "", &dst(filter)));
        let imported = import_result(d.path()).unwrap_or_else(|e| panic!("{e:?}"));
        let n = notes(&imported.report);
        let mut r = Res::new(imported.skin);
        r.view(&view());
        (r.frame(1_000), n)
    };
    let (plain, plain_notes) = frame(0);
    let (filtered, filtered_notes) = frame(1);
    assert_eq!(plain, filtered);
    assert!(plain.iter().any(|q| q.filter == Filter::NearestBase));
    let note = (Severity::Ignored, NoteKind::Ignored { field: "filter" });
    assert!(!plain_notes.contains(&note));
    assert!(filtered_notes.contains(&note));
}

/// The destination's stretch fits the whole graph against its texture of
/// 5 texels a bin and 5 a cell (70 bins at scale 20: 350 × 100): no
/// resize (9) centres it in a 520 × 300 rect, fit inside (1) keeps its
/// aspect 7 : 2 at the rect's width (520 × 148.57), and the trimmed
/// covering fit (3) cuts it to the rect.
#[test]
fn the_stretch_fits_the_whole_graph() {
    let bounds = |stretch: i32| {
        let dst = format!(
            r#"{{"id": "g", "stretch": {stretch}, "dst": [{{"x": 100, "y": 200, "w": 520, "h": 300}}]}}"#
        );
        let d = folder(&json_skin(r#"{"id": "g", "type": 1}"#, "", &dst));
        let imported = import_result(d.path()).unwrap_or_else(|e| panic!("{e:?}"));
        let mut r = Res::new(imported.skin);
        let mut v = view();
        // Every bin within 20 notes: scale 20.
        let bins = vec![[1; 9]; 70];
        v.timelines = Some(Timelines::new(TimeUs(1_000_000), bins, vec![9; 70], None).unwrap());
        r.view(&v);
        let (_, back) = graph_quads(&mut r, 1_000);
        back.iter().fold(
            [f32::MAX, f32::MAX, f32::MIN, f32::MIN],
            |[x0, y0, x1, y1], q| {
                let [x, y, w, h] = q.rect;
                [x0.min(x), y0.min(y), x1.max(x + w), y1.max(y + h)]
            },
        )
    };
    let near = |got: [f32; 4], want: [f32; 4]| {
        assert!(
            got.iter().zip(want).all(|(g, w)| (g - w).abs() < 0.01),
            "{got:?}, expected {want:?}"
        );
    };
    // The rect: x 100 to 620, y 580 to 880 (y up from 200, 300 tall).
    near(bounds(0), [100.0, 580.0, 620.0, 880.0]);
    near(bounds(9), [185.0, 680.0, 535.0, 780.0]);
    near(bounds(1), [100.0, 655.714, 620.0, 804.286]);
    near(bounds(3), [100.0, 580.0, 620.0, 880.0]);
}
