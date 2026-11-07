//! A result skin's gauge graph and timing distribution through the
//! importer (docs/stage3.md §3, Result; S10-09h-1): JSON and Lua read the
//! same fields into the same frames with the source's defaults, a colour
//! is plain hex or draws red with a note, the graph kept (C24); a
//! distribution past 4 096 columns drops with a note (C25); a `filter`
//! keeps either graph with a note (C26); and a distribution's stretch
//! fits the whole graph while a gauge graph's changes nothing.

mod common;

use common::result::{Res, folder, import_result, lua_result, ready, sheet};
use rules::GaugeValue;
use skin_beatoraja::{ImportFacts, NoteKind, Object, Report, Severity};
use skin_model::{
    BestView, DEVIATION_SLOTS, GRAPH_COLUMNS, GaugeColumn, GaugeHistory, GaugeLook, Quad,
    ResultView, ScreenKind, Skin,
};

/// The essential parts every fixture holds.
const ESSENTIALS_JSON: &str = r#"{"id": "ex", "dst": [{"x": 0, "y": 100, "w": 4, "h": 8}]},
    {"id": "dot", "op": [90], "dst": [{"x": 0, "y": 300, "w": 8, "h": 8}]}"#;

/// The gauge graph's region: x 100, y-up 600, 576 × 200 (top 280).
const GAUGE_DST: &str = r#""dst": [{"x": 100, "y": 600, "w": 576, "h": 200}]"#;
/// The distribution's region: x 100, y-up 200, 301 × 100 (top 780).
const TIMING_DST: &str = r#""dst": [{"x": 100, "y": 200, "w": 301, "h": 100}]"#;

/// A JSON result skin with `tables` (the graph records) and `dsts`.
fn json_skin(tables: &str, dsts: &str) -> String {
    format!(
        r#""value": [{{"id": "ex", "src": "s", "x": 0, "y": 32, "w": 40, "h": 8, "divx": 10, "digit": 4, "ref": 71}}],
           {tables}
           "destination": [{ESSENTIALS_JSON}, {dsts}]"#
    )
}

/// The Lua skin of the same parts: `tables` as Lua statements.
fn lua_skin(tables: &str, dsts: &str) -> String {
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

/// An imported skin and its folder, which its atlas reads.
struct Imported {
    _dir: tempfile::TempDir,
    skin: Skin,
    report: Report,
}

/// `json_skin` imported.
fn json(tables: &str, dsts: &str) -> Imported {
    let d = folder(&json_skin(tables, dsts));
    let i = import_result(d.path()).unwrap_or_else(|e| panic!("{e:?}"));
    Imported {
        _dir: d,
        skin: i.skin,
        report: i.report,
    }
}

/// `lua_skin` imported.
fn lua(tables: &str, dsts: &str) -> Imported {
    let d = sheet();
    common::write(d.path(), "s.luaskin", lua_skin(tables, dsts).as_bytes());
    let i = lua_result(d.path(), &ImportFacts::unavailable(ScreenKind::Result))
        .unwrap()
        .imported;
    Imported {
        _dir: d,
        skin: i.skin,
        report: i.report,
    }
}

/// `p` percent of the gauge.
fn pct(p: i64) -> GaugeValue {
    GaugeValue(p * GaugeValue::PERCENT)
}

/// 7-key windows: PERFECT ±20 ms, GREAT ±60, GOOD ±160, BAD ±220, the
/// empty POOR range −500 to +150.
const AREAS: [[i16; 2]; 5] = [[-20, 20], [-60, 60], [-160, 160], [-220, 220], [-500, 150]];

/// A ready view with a flat 50 % gauge of `look` (border 80 %) and no
/// deviation sample: the distribution draws its areas and ticks.
fn view(look: GaugeLook) -> ResultView {
    let flat = GaugeColumn {
        low: pct(50),
        high: pct(50),
        last: pct(50),
    };
    ResultView {
        gauge: Some(Box::new(GaugeHistory {
            columns: [flat; GRAPH_COLUMNS],
            max: pct(100),
            border: pct(80),
            look,
            reached: 288,
            no_input_from: None,
        })),
        deviations: Some(Box::new([0; DEVIATION_SLOTS])),
        timing_areas: Some(AREAS),
        timing: None,
        ..ready(1_000, true, BestView::Unavailable)
    }
}

/// The frame at `ms` over `view`.
fn frame(imported: Imported, view: &ResultView, ms: i64) -> Vec<Quad> {
    let mut r = Res::new(imported.skin);
    r.view(view);
    r.frame(ms)
}

/// The colour (8-bit RGBA) of the quad covering canvas point `(x, y)`;
/// `None` when no quad does or more than one does.
fn colour_at(quads: &[Quad], x: f32, y: f32) -> Option<[u8; 4]> {
    let mut hit = quads.iter().filter(|q| {
        let [qx, qy, qw, qh] = q.rect;
        x >= qx && x < qx + qw && y >= qy && y < qy + qh
    });
    let q = hit.next()?;
    hit.next()
        .is_none()
        .then(|| q.color.map(|c| (c * 255.0).round() as u8))
}

/// The gauge graph's back below the border, at its bottom left.
fn gauge_back(quads: &[Quad]) -> Option<[u8; 4]> {
    colour_at(quads, 101.0, 470.0)
}

/// The distribution's PERFECT area at ms 5 (column 155, no tick).
fn perfect(quads: &[Quad]) -> Option<[u8; 4]> {
    colour_at(quads, 255.5, 830.0)
}

/// The dropped, ignored and fallback notes of a report.
fn notes(r: &Report) -> Vec<(Severity, NoteKind)> {
    let mut n: Vec<_> = r
        .notes
        .iter()
        .filter(|n| {
            matches!(
                n.severity,
                Severity::Dropped | Severity::Ignored | Severity::Fallback
            )
        })
        .map(|n| (n.severity, n.kind))
        .collect();
    n.sort_by_key(|n| format!("{n:?}"));
    n
}

const BOTH_JSON: &str =
    r#""gaugegraph": [{"id": "gg"}], "timingdistributiongraph": [{"id": "td"}],"#;
const BOTH_LUA: &str = "skin.gaugegraph = { { id = \"gg\" } }\n\
                        skin.timingdistributiongraph = { { id = \"td\" } }";

fn both_dsts_json() -> String {
    format!(r#"{{"id": "gg", {GAUGE_DST}}}, {{"id": "td", {TIMING_DST}}}"#)
}
const BOTH_DSTS_LUA: &str = "{ id = \"gg\", dst = { { x = 100, y = 600, w = 576, h = 200 } } },\n\
     { id = \"td\", dst = { { x = 100, y = 200, w = 301, h = 100 } } }";

/// C24: a JSON and a Lua skin declaring a gauge graph and a distribution
/// with the defaults draw the same frames while the gauge line reveals,
/// with no note about either graph; the defaults are the source's: the
/// NORMAL look's back `004400` below the border, the distribution's 301
/// columns (one a pixel) with PERFECT `000088` at ms 5 and GOOD `888800`
/// at ms −130, and its ticks.
#[test]
fn json_and_lua_graphs_draw_the_same_frames_with_the_defaults() {
    let v = view(GaugeLook::Normal);
    let j = json(BOTH_JSON, &both_dsts_json());
    let l = lua(BOTH_LUA, BOTH_DSTS_LUA);
    assert_eq!(notes(&j.report), notes(&l.report));
    assert!(!notes(&j.report).iter().any(|(_, k)| matches!(
        k,
        NoteKind::Unsupported {
            object: Object::GaugeGraph | Object::TimingDistribution
        } | NoteKind::InvalidColour { .. }
    )));
    let mut a = Res::new(j.skin);
    let mut b = Res::new(l.skin);
    a.view(&v);
    b.view(&v);
    for ms in [0, 750, 2_000] {
        assert_eq!(a.frame(ms), b.frame(ms), "{ms} ms");
    }
    let q = a.frame(2_000);
    assert_eq!(gauge_back(&q), Some([0x00, 0x44, 0x00, 0xff]));
    assert_eq!(perfect(&q), Some([0x00, 0x00, 0x88, 0xff]));
    assert_eq!(colour_at(&q, 120.5, 830.0), Some([0x88, 0x88, 0x00, 0xff]));
    // A tick, black at a quarter, over column 150's top rows.
    assert_eq!(colour_at(&q, 250.5, 781.0).map(|c| c[3]), Some(0xff));
}

/// C24: the gauge graph's colours. The named ones replace the defaults
/// per look, the end-threshold looks sharing the border line and back;
/// the 24-entry `color` array gives each look's border line, border
/// back, line and back in turn, a missing entry the source's black, a bad
/// one red with a note.
#[test]
fn the_gauge_graph_reads_its_colours() {
    let dst = format!(r#"{{"id": "gg", {GAUGE_DST}}}"#);
    let named = json(
        r#""gaugegraph": [{"id": "gg", "grooveFailBGColor": "123456", "exHardBGColor": "abcdef80"}],"#,
        &dst,
    );
    let q = frame(named, &view(GaugeLook::Normal), 2_000);
    assert_eq!(gauge_back(&q), Some([0x12, 0x34, 0x56, 0xff]));
    let named = json(
        r#""gaugegraph": [{"id": "gg", "exHardBGColor": "abcdef80"}],"#,
        &dst,
    );
    let q = frame(named, &view(GaugeLook::ExHard), 2_000);
    assert_eq!(gauge_back(&q), Some([0xab, 0xcd, 0xef, 0x80]));
    // 12 entries: the first three looks; entry 11 (the NORMAL look's
    // back) does not parse.
    let entries = (0..12)
        .map(|i| {
            if i == 11 {
                "\"zzzzzz\"".to_owned()
            } else {
                format!("\"{:02x}0000\"", i + 1)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let array = |look| {
        let imported = json(
            &format!(r#""gaugegraph": [{{"id": "gg", "color": [{entries}]}}],"#),
            &dst,
        );
        let n = notes(&imported.report);
        (frame(imported, &view(look), 2_000), n)
    };
    let (q, n) = array(GaugeLook::Easy);
    assert_eq!(gauge_back(&q), Some([0x08, 0x00, 0x00, 0xff]));
    let (q, _) = array(GaugeLook::Normal);
    assert_eq!(gauge_back(&q), Some([0xff, 0x00, 0x00, 0xff]));
    assert!(n.contains(&(
        Severity::Fallback,
        NoteKind::InvalidColour { field: "color" }
    )));
    let (q, _) = array(GaugeLook::Hard);
    assert_eq!(gauge_back(&q), Some([0x00, 0x00, 0x00, 0xff]));
}

/// C24: a colour is an optional `#` and exactly 6 or 8 hex digits:
/// `ff00ff` and `#ff00ff80` draw as written; `ff00`, `ff00ff8`, `+10000`,
/// `zzzzzz` and `ff00ffaabb` draw opaque red with a note, the graph kept;
/// JSON and Lua alike.
#[test]
fn a_colour_is_plain_hex_or_red_with_a_note() {
    let cases: [(&str, [u8; 4]); 7] = [
        ("ff00ff", [0xff, 0x00, 0xff, 0xff]),
        ("#ff00ff80", [0xff, 0x00, 0xff, 0x80]),
        ("ff00", [0xff, 0x00, 0x00, 0xff]),
        ("ff00ff8", [0xff, 0x00, 0x00, 0xff]),
        ("+10000", [0xff, 0x00, 0x00, 0xff]),
        ("zzzzzz", [0xff, 0x00, 0x00, 0xff]),
        ("ff00ffaabb", [0xff, 0x00, 0x00, 0xff]),
    ];
    let note = (
        Severity::Fallback,
        NoteKind::InvalidColour { field: "PGColor" },
    );
    let dst = format!(r#"{{"id": "td", {TIMING_DST}}}"#);
    let lua_dst = "{ id = \"td\", dst = { { x = 100, y = 200, w = 301, h = 100 } } }";
    for (i, (s, want)) in cases.into_iter().enumerate() {
        let valid = i < 2;
        let j = json(
            &format!(r#""timingdistributiongraph": [{{"id": "td", "PGColor": "{s}"}}],"#),
            &dst,
        );
        assert_eq!(notes(&j.report).contains(&note), !valid, "{s}");
        let l = lua(
            &format!("skin.timingdistributiongraph = {{ {{ id = \"td\", PGColor = \"{s}\" }} }}"),
            lua_dst,
        );
        assert_eq!(notes(&l.report).contains(&note), !valid, "{s} (Lua)");
        let v = view(GaugeLook::Normal);
        let (qj, ql) = (frame(j, &v, 0), frame(l, &v, 0));
        assert_eq!(qj, ql, "{s}");
        assert_eq!(perfect(&qj), Some(want), "{s}");
    }
}

/// C25: a distribution of `width / lineWidth` columns imports up to 4 096
/// and drops with a note past it (owner decision H4): width 4 096, and
/// 8 192 at line width 2, are kept; 4 097, and 8 194 at line width 2, are
/// dropped.
#[test]
fn a_distribution_past_4096_columns_drops_with_a_note() {
    let dst = format!(r#"{{"id": "td", {TIMING_DST}}}"#);
    let note = (
        Severity::Dropped,
        NoteKind::UnsupportedField { field: "width" },
    );
    for (width, line, kept) in [
        (4_096, 1, true),
        (8_192, 2, true),
        (4_097, 1, false),
        (8_194, 2, false),
    ] {
        let j = json(
            &format!(
                r#""timingdistributiongraph": [{{"id": "td", "width": {width}, "lineWidth": {line}}}],"#
            ),
            &dst,
        );
        assert_eq!(notes(&j.report).contains(&note), !kept, "{width} / {line}");
        let q = frame(j, &view(GaugeLook::Normal), 0);
        assert_eq!(
            perfect(&q).is_some() || colour_at(&q, 250.5, 830.0).is_some(),
            kept,
            "{width} / {line}"
        );
    }
}

/// C26: a destination `filter` keeps a gauge graph and a distribution,
/// drawn as without it, each with a note.
#[test]
fn a_filter_keeps_either_graph_with_a_note() {
    let dsts = |filter: i32| {
        format!(
            r#"{{"id": "gg", "filter": {filter}, {GAUGE_DST}}}, {{"id": "td", "filter": {filter}, {TIMING_DST}}}"#
        )
    };
    let (plain, filtered) = (json(BOTH_JSON, &dsts(0)), json(BOTH_JSON, &dsts(1)));
    let note = (Severity::Ignored, NoteKind::Ignored { field: "filter" });
    assert_eq!(
        notes(&plain.report).iter().filter(|&&n| n == note).count(),
        0
    );
    assert_eq!(
        notes(&filtered.report)
            .iter()
            .filter(|&&n| n == note)
            .count(),
        2
    );
    let v = view(GaugeLook::Normal);
    let (a, b) = (frame(plain, &v, 2_000), frame(filtered, &v, 2_000));
    assert_eq!(a, b);
    assert!(gauge_back(&a).is_some() && perfect(&a).is_some());
}

/// The bounds of `quads` drawn in `[x0, x1] × [y0, y1]` canvas space.
fn bounds(quads: &[Quad], [x0, y0, x1, y1]: [f32; 4]) -> [f32; 4] {
    quads
        .iter()
        .filter(|q| q.rect[0] >= x0 && q.rect[1] >= y0 && q.rect[0] < x1 && q.rect[1] < y1)
        .fold(
            [f32::MAX, f32::MAX, f32::MIN, f32::MIN],
            |[a, b, c, d], q| {
                let [x, y, w, h] = q.rect;
                [a.min(x), b.min(y), c.max(x + w), d.max(y + h)]
            },
        )
}

/// A distribution's stretch fits the whole graph against its texture of
/// `gx` × scale texels (301 × 10 with no sample): no resize (9) centres it
/// in a 602 × 200 rect at (250.5, 775), the trimmed covering fit (3)
/// fills the rect. A gauge graph's texture is its rect, so every stretch
/// draws its stretch-0 frame.
#[test]
fn a_distributions_stretch_fits_it_and_a_gauge_graphs_changes_nothing() {
    let td = |stretch: i32| {
        let dst = format!(
            r#"{{"id": "td", "stretch": {stretch}, "dst": [{{"x": 100, "y": 200, "w": 602, "h": 200}}]}}"#
        );
        let q = frame(
            json(r#""timingdistributiongraph": [{"id": "td"}],"#, &dst),
            &view(GaugeLook::Normal),
            0,
        );
        bounds(&q, [50.0, 600.0, 1_000.0, 1_000.0])
    };
    let near = |got: [f32; 4], want: [f32; 4]| {
        assert!(
            got.iter().zip(want).all(|(g, w)| (g - w).abs() < 0.01),
            "{got:?}, expected {want:?}"
        );
    };
    near(td(0), [100.0, 680.0, 702.0, 880.0]);
    near(td(9), [250.5, 775.0, 551.5, 785.0]);
    near(td(3), [100.0, 680.0, 702.0, 880.0]);
    let gg = |stretch: i32| {
        let dst = format!(r#"{{"id": "gg", "stretch": {stretch}, {GAUGE_DST}}}"#);
        frame(
            json(r#""gaugegraph": [{"id": "gg"}],"#, &dst),
            &view(GaugeLook::Normal),
            2_000,
        )
    };
    let plain = gg(0);
    for stretch in 1..=10 {
        assert_eq!(gg(stretch), plain, "stretch {stretch}");
    }
}
