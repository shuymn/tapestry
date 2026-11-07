//! A result skin's gauge graph and timing distribution through the
//! importer (docs/stage3.md §3, Result; S10-09h-1): JSON and Lua read the
//! same fields into the same frames with the source's defaults, a colour
//! is plain hex or draws red with a note, the graph kept (C24); a
//! distribution past 4 096 columns drops with a note (C25); a `filter`
//! keeps either graph with a note (C26); and a distribution's stretch
//! fits the whole graph while a gauge graph's changes nothing.

mod common;

use common::result::{Res, folder, graph_json, graph_lua, import_result, lua_result, ready, sheet};
use rules::GaugeValue;
use skin_beatoraja::{ImportFacts, NoteKind, Object, Report, Severity};
use skin_model::{
    BestView, DEVIATION_SLOTS, GRAPH_COLUMNS, GaugeColumn, GaugeHistory, GaugeLook, Quad,
    ResultView, ScreenKind, Skin,
};

/// The gauge graph's region: x 100, y-up 600, 576 × 200 (top 280).
const GAUGE_DST: &str = r#""dst": [{"x": 100, "y": 600, "w": 576, "h": 200}]"#;
/// The distribution's region: x 100, y-up 200, 301 × 100 (top 780).
const TIMING_DST: &str = r#""dst": [{"x": 100, "y": 200, "w": 301, "h": 100}]"#;

/// An imported skin and its folder, which its atlas reads.
struct Imported {
    _dir: tempfile::TempDir,
    skin: Skin,
    report: Report,
}

/// `json_skin` imported.
fn json(tables: &str, dsts: &str) -> Imported {
    let d = folder(&graph_json(tables, dsts));
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
    common::write(d.path(), "s.luaskin", graph_lua(tables, dsts).as_bytes());
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

/// An opaque colour whose three channels differ.
fn opaque(r: u8, g: u8, b: u8) -> [u8; 4] {
    [r, g, b, 0xff]
}

/// `c` as the skin writes it, `rrggbbaa`.
fn hex([r, g, b, a]: [u8; 4]) -> String {
    format!("{r:02x}{g:02x}{b:02x}{a:02x}")
}

/// Colour fields as a JSON record's members and as a Lua table's.
fn fields(fields: &[(&str, [u8; 4])]) -> (String, String) {
    let join = |f: &dyn Fn(&str, String) -> String| {
        fields
            .iter()
            .map(|&(n, c)| f(n, hex(c)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    (
        join(&|n, c| format!(r#""{n}": "{c}""#)),
        join(&|n, c| format!("{n} = \"{c}\"")),
    )
}

const GAUGE_LUA_DST: &str = "{ id = \"gg\", dst = { { x = 100, y = 600, w = 576, h = 200 } } }";

/// The gauge graph's roles for `look` under the record members `json`
/// and `lua`, its JSON and Lua frames equal.
fn gauge_case(json_members: &str, lua_members: &str, look: GaugeLook) -> Roles {
    let dst = format!(r#"{{"id": "gg", {GAUGE_DST}}}"#);
    let j = json(
        &format!(r#""gaugegraph": [{{"id": "gg", {json_members}}}],"#),
        &dst,
    );
    let l = lua(
        &format!("skin.gaugegraph = {{ {{ id = \"gg\", {lua_members} }} }}"),
        GAUGE_LUA_DST,
    );
    let v = split_view(look);
    let (qj, ql) = (frame(j, &v, 2_000), frame(l, &v, 2_000));
    assert_eq!(qj, ql, "{look:?}");
    gauge_roles(&qj)
}

/// A ready view of `look` whose first 144 columns sit flat at 50 % (the
/// line below the 80 % border) and the rest at 90 % (above it).
fn split_view(look: GaugeLook) -> ResultView {
    let at = |p| GaugeColumn {
        low: pct(p),
        high: pct(p),
        last: pct(p),
    };
    let mut columns = [at(50); GRAPH_COLUMNS];
    for c in &mut columns[144..] {
        *c = at(90);
    }
    columns[144].low = pct(50);
    let mut v = view(look);
    v.gauge.as_mut().unwrap().columns = columns;
    v
}

/// A gauge graph's colours: the back below and above the border, then
/// every line colour below and above it.
type Roles = ([u8; 4], [u8; 4], Vec<[u8; 4]>, Vec<[u8; 4]>);

/// The gauge graph's colours in a frame of the region (x 100, top 280,
/// 576 × 200, border at y 321.6): the back below the border, the back
/// above it, every line piece below the border, every one above it.
fn gauge_roles(quads: &[Quad]) -> Roles {
    let rgb = |q: &Quad| q.color.map(|c| (c * 255.0).round() as u8);
    let inside = |q: &&Quad| q.rect[0] >= 99.0 && q.rect[1] >= 279.0 && q.rect[1] < 481.0;
    let backs: Vec<&Quad> = quads
        .iter()
        .filter(inside)
        .filter(|q| q.rect[2] == 576.0)
        .collect();
    let low = backs
        .iter()
        .max_by(|a, b| a.rect[1].total_cmp(&b.rect[1]))
        .unwrap();
    let high = backs
        .iter()
        .min_by(|a, b| a.rect[1].total_cmp(&b.rect[1]))
        .unwrap();
    let yb = 480.0 - 0.8 * 198.0;
    let mut below = Vec::new();
    let mut above = Vec::new();
    for q in quads.iter().filter(inside).filter(|q| q.rect[2] < 576.0) {
        let c = rgb(q);
        if q.rect[1] >= yb - 1e-3 {
            below.push(c);
        } else if q.rect[1] + q.rect[3] <= yb + 1e-3 {
            above.push(c);
        }
    }
    for v in [&mut below, &mut above] {
        v.sort_unstable();
        v.dedup();
    }
    (rgb(low), rgb(high), below, above)
}

/// C24: every gauge graph colour role reaches its look, JSON and Lua
/// alike. The 14 named fields, each a distinct opaque colour: the first
/// three looks draw their own back below the border and line below it,
/// and share `borderColor` (back above) and `borderlineColor` (line
/// above); the last three draw their own back and line on both sides.
/// The 24-entry array gives look `i` entries `4i` to `4i + 3`: border
/// line, border back, line, back.
#[test]
fn every_gauge_graph_colour_reaches_its_role() {
    let named: Vec<(&str, [u8; 4])> = [
        "assistClearBGColor",
        "assistAndEasyFailBGColor",
        "grooveFailBGColor",
        "grooveClearAndHardBGColor",
        "exHardBGColor",
        "hazardBGColor",
        "assistClearLineColor",
        "assistAndEasyFailLineColor",
        "grooveFailLineColor",
        "grooveClearAndHardLineColor",
        "exHardLineColor",
        "hazardLineColor",
        "borderlineColor",
        "borderColor",
    ]
    .into_iter()
    .zip(0_u8..)
    .map(|(n, i)| (n, opaque(10 + i, 100 + i, 200 + i)))
    .collect();
    let (json_fields, lua_fields) = fields(&named);
    let c = |i: usize| named[i].1;
    for (i, look) in GaugeLook::ALL.into_iter().enumerate() {
        let (back, border_back, line, border_line) = if i < 3 {
            (c(i), c(13), c(6 + i), c(12))
        } else {
            (c(i), c(i), c(6 + i), c(6 + i))
        };
        assert_eq!(
            gauge_case(&json_fields, &lua_fields, look),
            (back, border_back, vec![line], vec![border_line]),
            "{look:?}"
        );
    }
    let entries: Vec<[u8; 4]> = (0..24_u8)
        .map(|i| opaque(10 + i, 60 + i, 110 + i))
        .collect();
    let array = entries
        .iter()
        .map(|&e| format!("\"{}\"", hex(e)))
        .collect::<Vec<_>>()
        .join(", ");
    for (i, look) in GaugeLook::ALL.into_iter().enumerate() {
        let e = |k: usize| entries[4 * i + k];
        assert_eq!(
            gauge_case(
                &format!(r#""color": [{array}]"#),
                &format!("color = {{ {array} }}"),
                look
            ),
            (e(3), e(1), vec![e(2)], vec![e(0)]),
            "{look:?}"
        );
    }
}

/// Judge areas small enough that a 301-column graph shows all five:
/// PERFECT ±10 ms, GREAT ±20, GOOD ±30, BAD ±40, the empty POOR range
/// −60 to +60.
const SMALL_AREAS: [[i16; 2]; 5] = [[-10, 10], [-20, 20], [-30, 30], [-40, 40], [-60, 60]];

/// A ready view with the timing statistics of a −1.49 ms mean and a
/// 12.34 ms σ (the average at ms −1, the ±σ lines at ms 11 and −13), five
/// deviations at ms −70 (a bar half the height at scale 10), and
/// [`SMALL_AREAS`].
fn stats_view() -> ResultView {
    let mut d = [0; DEVIATION_SLOTS];
    d[80] = 5;
    ResultView {
        deviations: Some(Box::new(d)),
        timing_areas: Some(SMALL_AREAS),
        timing: Some(skin_model::TimingStats {
            samples: 5,
            mean_x100: -149,
            sd_x100: 1_234,
        }),
        ..view(GaugeLook::Normal)
    }
}

/// The distribution's colour at ms `ms`, `row` (from the top, 10 pixels
/// each at scale 10) in the region of x 100, top 780, 301 × 100.
fn at_ms(quads: &[Quad], ms: i32, row: f32) -> Option<[u8; 4]> {
    colour_at(quads, 250.5 + ms as f32, 780.0 + row * 10.0 + 5.0)
}

/// The eight distribution colour fields, each a distinct opaque colour.
fn timing_fields() -> Vec<(&'static str, [u8; 4])> {
    [
        "graphColor",
        "averageColor",
        "devColor",
        "PGColor",
        "GRColor",
        "GDColor",
        "BDColor",
        "PRColor",
    ]
    .into_iter()
    .zip(0_u8..)
    .map(|(n, i)| (n, opaque(20 + i, 120 + i, 220 - i)))
    .collect()
}

/// A distribution's JSON and Lua records with `colours` and `more`.
fn timing_records(
    colours: &[(&str, [u8; 4])],
    more_json: &str,
    more_lua: &str,
) -> (String, String) {
    let (j, l) = fields(colours);
    (
        format!(r#""timingdistributiongraph": [{{"id": "td", {j}{more_json}}}],"#),
        format!("skin.timingdistributiongraph = {{ {{ id = \"td\", {l}{more_lua} }} }}"),
    )
}

const TIMING_LUA_DST: &str = "{ id = \"td\", dst = { { x = 100, y = 200, w = 301, h = 100 } } }";

/// C24: every distribution colour reaches its role, JSON and Lua alike:
/// the bar at ms −70, the average line at ms −1, the ±σ line at ms 11,
/// and the areas PERFECT (ms 5), GREAT (15), GOOD (25), BAD (35) and POOR
/// (55), each away from the ticks.
#[test]
fn every_distribution_colour_reaches_its_role() {
    let fields = timing_fields();
    let (tj, tl) = timing_records(&fields, "", "");
    let dst = format!(r#"{{"id": "td", {TIMING_DST}}}"#);
    let v = stats_view();
    let qj = frame(json(&tj, &dst), &v, 0);
    let ql = frame(lua(&tl, TIMING_LUA_DST), &v, 0);
    assert_eq!(qj, ql);
    let c = |i: usize| Some(fields[i].1);
    assert_eq!(at_ms(&qj, -70, 9.0), c(0), "bar");
    assert_eq!(at_ms(&qj, -1, 5.0), c(1), "average");
    assert_eq!(at_ms(&qj, 11, 5.0), c(2), "σ");
    for (k, ms) in [5, 15, 25, 35, 55].into_iter().enumerate() {
        assert_eq!(at_ms(&qj, ms, 5.0), c(3 + k), "area {k}");
    }
}

/// `drawAverage` and `drawDev` are each on only at 1, JSON and Lua alike:
/// at −1, 0, 1 and 2 (the other flag 0), the average column at ms −1 and
/// the σ column at ms 11 show their line only at 1, else the area under
/// them (PERFECT, GREAT).
#[test]
fn the_line_flags_are_on_only_at_one() {
    let fields = timing_fields();
    let c = |i: usize| Some(fields[i].1);
    let dst = format!(r#"{{"id": "td", {TIMING_DST}}}"#);
    let v = stats_view();
    for flag in [-1, 0, 1, 2] {
        for (name, ms, line, area) in [("drawAverage", -1, 1, 3), ("drawDev", 11, 2, 4)] {
            let other = if name == "drawAverage" {
                "drawDev"
            } else {
                "drawAverage"
            };
            let (tj, tl) = timing_records(
                &fields,
                &format!(r#", "{name}": {flag}, "{other}": 0"#),
                &format!(", {name} = {flag}, {other} = 0"),
            );
            let qj = frame(json(&tj, &dst), &v, 0);
            let ql = frame(lua(&tl, TIMING_LUA_DST), &v, 0);
            assert_eq!(qj, ql, "{name} {flag}");
            let want = if flag == 1 { c(line) } else { c(area) };
            assert_eq!(at_ms(&qj, ms, 5.0), want, "{name} {flag}");
        }
    }
}

/// C25: the distribution's columns are `width / lineWidth`, `lineWidth`
/// clamped to `1..=width`, and its charge follows them: width 301 at
/// line width 2 makes 150 columns (centre 75, each 301/150 pixels), so
/// PERFECT ends at ms 10 (column 85) and GREAT starts at ms 11 (column
/// 86), charged 5·150 + 11; line widths 0 and −5 clamp to 1 (301
/// columns, ms 10 PERFECT at x 260.5, ms 11 GREAT), charged 1 516; line
/// width 1 000 clamps to 301, one column of ms 0 over the whole width,
/// charged 5 + 11. The number of the essentials adds 4.
#[test]
fn the_columns_divide_and_clamp_as_the_source() {
    let fields = timing_fields();
    let c = |i: usize| Some(fields[i].1);
    let dst = format!(r#"{{"id": "td", {TIMING_DST}}}"#);
    let mut v = stats_view();
    v.timing = None;
    let case = |line: i32| {
        let (tj, _) = timing_records(&fields, &format!(r#", "lineWidth": {line}"#), "");
        let j = json(&tj, &dst);
        let charge = j.report.usage.widget_quads;
        (frame(j, &v, 0), charge)
    };
    let (q, charge) = case(2);
    assert_eq!(charge, 4 + 5 * 150 + 11);
    let col = |j: f32| 100.0 + 301.0 * (j + 0.5) / 150.0;
    assert_eq!(colour_at(&q, col(85.0), 830.0), c(3));
    assert_eq!(colour_at(&q, col(86.0), 830.0), c(4));
    for line in [0, -5] {
        let (q, charge) = case(line);
        assert_eq!(charge, 4 + 1_516, "{line}");
        assert_eq!(at_ms(&q, 10, 5.0), c(3), "{line}");
        assert_eq!(at_ms(&q, 11, 5.0), c(4), "{line}");
    }
    let (q, charge) = case(1_000);
    assert_eq!(charge, 4 + 5 + 11);
    assert_eq!(colour_at(&q, 101.0, 830.0), c(3));
    assert_eq!(colour_at(&q, 400.0, 830.0), c(3));
}

/// A graph's destination keeps every key: a gauge graph and a
/// distribution moving from x 100 to 300 over 1 s are at x 200 at 500 ms.
#[test]
fn a_graphs_keys_move_it() {
    let dsts = r#"{"id": "gg", "dst": [{"time": 0, "x": 100, "y": 600, "w": 576, "h": 200}, {"time": 1000, "x": 300, "y": 600, "w": 576, "h": 200}]},
        {"id": "td", "dst": [{"time": 0, "x": 100, "y": 200, "w": 301, "h": 100}, {"time": 1000, "x": 300, "y": 200, "w": 301, "h": 100}]}"#;
    let q = frame(json(BOTH_JSON, dsts), &view(GaugeLook::Normal), 500);
    let left = |top: f32| {
        q.iter()
            .filter(|q| q.rect[1] >= top && q.rect[1] < top + 100.0)
            .map(|q| q.rect[0])
            .fold(f32::MAX, f32::min)
    };
    assert_eq!(left(280.0), 200.0);
    assert_eq!(left(780.0), 200.0);
}
