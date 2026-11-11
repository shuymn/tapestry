//! A play skin's notes graph, BPM graph and visualizers through the
//! importer (s10-10-design.md C31 and C32; design.md "Rendering and
//! Skins", play graphs and visualizers): JSON and Lua read the same fields
//! into the same frames with the source's defaults, judge graph types 1 and
//! 2 drop with a note, a field the source clamps is clamped, and a
//! visualizer past the model's caps drops with a note, the skin kept.
//! Every expected colour and place is computed here from the source's
//! defaults (`skin/json/JsonSkin.java:263-321` at `0.8.9`) and rules.

mod common;

use bms_model::{Chart, ChartBuilder, ChartProfile, ChartSummary, Lane, Tempo, TimeUs};
use common::import_sized;
use common::play::{Drawn, MS};
use skin_beatoraja::{Cancel, Config, NoteKind, Report, Severity, read_lua_header};
use skin_model::{Filter, PlayScore, Quad, Skin};
use tempfile::TempDir;

/// PERFECT ±20 ms, GREAT ±60, GOOD ±100, BAD ±120, the empty POOR range
/// ±150: within the default 150 ms either side, each band of its own.
const AREAS: [[i16; 2]; 5] = [[-20, 20], [-60, 60], [-100, 100], [-120, 120], [-150, 150]];

/// A play skin of `tables` (JSON members) and `dsts` (destination
/// objects), imported.
fn json(tables: &str, dsts: &str) -> (TempDir, Skin, Report) {
    let l = import_sized(
        [1_920, 1_080],
        &format!("{tables} \"destination\": [{dsts}]"),
        &[],
    );
    let i = match l.result {
        Ok(i) => i,
        Err(e) => panic!("{e:?}"),
    };
    (l.dir, i.skin, i.report)
}

/// The same skin written in Lua: `tables` and `dsts` as Lua statements and
/// destination tables.
fn lua(tables: &str, dsts: &str) -> (TempDir, Skin, Report) {
    let d = tempfile::tempdir().unwrap();
    let entry = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         {tables}\n\
         skin.destination = {{ {dsts} }}\n\
         return skin\n"
    );
    common::write(d.path(), "s.luaskin", entry.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let i = common::lua_play(&h, &Config::default(), &cancel)
        .unwrap()
        .imported;
    (d, i.skin, i.report)
}

/// 100 taps on lane 1 from 1 s, 10 ms apart.
fn taps() -> Chart {
    common::play::chart(100)
}

/// The colour (8-bit straight RGBA) of the last quad covering `(x, y)`,
/// the one drawn on top.
fn top(quads: &[Quad], x: f32, y: f32) -> Option<[u8; 4]> {
    quads
        .iter()
        .rev()
        .find(|q| {
            let [qx, qy, qw, qh] = q.rect;
            x >= qx && x < qx + qw && y >= qy && y < qy + qh
        })
        .map(|q| q.color.map(|c| (c * 255.0).round() as u8))
}

/// The vertical middle of every quad together.
fn middle(quads: &[Quad]) -> f32 {
    let lo = quads.iter().map(|q| q.rect[1]).fold(f32::MAX, f32::min);
    let hi = quads
        .iter()
        .map(|q| q.rect[1] + q.rect[3])
        .fold(f32::MIN, f32::max);
    f32::midpoint(lo, hi)
}

fn rgba(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

/// The dropped, ignored and fallback notes of a report.
fn notes(r: &Report) -> Vec<(Severity, NoteKind)> {
    r.notes
        .iter()
        .filter(|n| {
            matches!(
                n.severity,
                Severity::Dropped | Severity::Ignored | Severity::Fallback
            )
        })
        .map(|n| (n.severity, n.kind))
        .collect()
}

/// `skin` over 100 taps with the windows [`AREAS`] and the ring holding
/// `offsets`, oldest first: its quads at 2 s.
fn visualized(skin: Skin, offsets: &[i32]) -> Vec<Quad> {
    let score = PlayScore {
        areas: Some(AREAS),
        ..PlayScore::NONE
    };
    let mut d = Drawn::over_score(skin, &taps(), score);
    for &v in offsets {
        d.record(v);
    }
    d.frame(2_000, &[])
}

/// C31: a timing visualizer of the source's defaults, 301 wide over 150 ms
/// either side, so a texel a pixel. From its left edge `x0`: the bands by
/// texel, the empty POOR range (0-29) `000000`, BAD (30-49) `880000`, GOOD
/// (50-89) `888800`, GREAT (90-129) `008800`, PERFECT (130-149) `000088`,
/// and on the right BAD (251-270) and the empty POOR range (271-300), each
/// probed off the grid lines (every 10th texel); and the line of an
/// offset of +40 ms, early to the right, at `x0 + (301 − 1)/2 + 40 = x0 +
/// 190`, in `00FF00` at its full alpha as the newest of 100.
#[test]
fn a_timing_visualizer_draws_the_source_defaults_in_json_and_lua() {
    let (_j, json_skin, json_report) = json(
        r#""timingvisualizer": [{"id": "tv"}],"#,
        r#"{"id": "tv", "dst": [{"x": 100, "y": 1000, "w": 301, "h": 20}]}"#,
    );
    let (_l, lua_skin, lua_report) = lua(
        "skin.timingvisualizer = { { id = \"tv\" } }",
        "{ id = \"tv\", dst = { { x = 100, y = 1000, w = 301, h = 20 } } }",
    );
    assert_eq!(notes(&json_report), []);
    assert_eq!(notes(&lua_report), []);
    let q = visualized(json_skin, &[40]);
    assert_eq!(q, visualized(lua_skin, &[40]));
    let y = middle(&q);
    let x0 = 100.0;
    for (texel, want) in [
        (25.0, 0x0000_00ff),
        (35.0, 0x8800_00ff),
        (55.0, 0x8888_00ff),
        (95.0, 0x0088_00ff),
        (135.0, 0x0000_88ff),
        (265.0, 0x8800_00ff),
        (285.0, 0x0000_00ff),
    ] {
        assert_eq!(top(&q, x0 + texel + 0.5, y), Some(rgba(want)), "{texel}");
    }
    assert_eq!(top(&q, x0 + 190.5, y), Some(rgba(0x00ff_00ff)));
}

/// C31: a hit-error visualizer of the source's defaults: a 301 × 60 pixmap
/// (`windowLength` 30) a pixel a texel, over 150 ms either side. An offset
/// of +40 ms, early to the left, is the line at column `(301 − 1)/2 − 40 =
/// 110` in GREAT's band colour `F2CB3080` (`colorMode` 1); the average,
/// `0 + ⌊0.1 · 40⌋ = 4`, the line at `150 − 4 = 146` in `FF0000FF`
/// (`emaMode` 1); the centre at 150 in `FFFFFFFF`.
#[test]
fn a_hit_error_visualizer_draws_the_source_defaults_in_json_and_lua() {
    let (_j, json_skin, json_report) = json(
        r#""hiterrorvisualizer": [{"id": "he"}],"#,
        r#"{"id": "he", "dst": [{"x": 100, "y": 800, "w": 301, "h": 60}]}"#,
    );
    let (_l, lua_skin, lua_report) = lua(
        "skin.hiterrorvisualizer = { { id = \"he\" } }",
        "{ id = \"he\", dst = { { x = 100, y = 800, w = 301, h = 60 } } }",
    );
    assert_eq!(notes(&json_report), []);
    assert_eq!(notes(&lua_report), []);
    let q = visualized(json_skin, &[40]);
    assert_eq!(q, visualized(lua_skin, &[40]));
    let y = middle(&q);
    let x0 = 100.0;
    assert_eq!(top(&q, x0 + 110.5, y), Some(rgba(0xf2cb_3080)));
    assert_eq!(top(&q, x0 + 146.5, y), Some(rgba(0xff00_00ff)));
    assert_eq!(top(&q, x0 + 150.5, y), Some(rgba(0xffff_ffff)));
    assert_eq!(top(&q, x0 + 120.5, y), None);
}

/// C31: the fields the source clamps are clamped, with no note: a line
/// width of 6 (one real skin's) is 4, so the line of +40 ms covers columns
/// `(301 − 4)/2 − 40 = 108` to 111; a window of 0 is 1 and of 101 is 100,
/// both drawn. Flags are on only at 1: `colorMode` 2 colours a line by age
/// in `lineColor`, here `00FF00FF` at its full alpha as the newest of a
/// window of 1 (`a · i / (wl/2)` = 2, clamped to 1).
#[test]
fn hit_error_fields_clamp_as_the_source_does() {
    let (_d, skin, report) = json(
        r#""hiterrorvisualizer": [{"id": "he", "lineWidth": 6, "emaMode": 0}],"#,
        r#"{"id": "he", "dst": [{"x": 100, "y": 800, "w": 301, "h": 60}]}"#,
    );
    assert_eq!(notes(&report), []);
    let q = visualized(skin, &[40]);
    let y = middle(&q);
    let line = Some(rgba(0xf2cb_3080));
    assert_eq!(top(&q, 100.0 + 107.5, y), None);
    assert_eq!(top(&q, 100.0 + 108.5, y), line);
    assert_eq!(top(&q, 100.0 + 111.5, y), line);
    assert_eq!(top(&q, 100.0 + 112.5, y), None);
    for window in [0, 101] {
        let (_d, skin, report) = json(
            &format!(
                r#""hiterrorvisualizer": [{{"id": "he", "windowLength": {window}, "colorMode": 2, "lineColor": "00FF00FF", "emaMode": 0}}],"#
            ),
            r#"{"id": "he", "dst": [{"x": 100, "y": 800, "w": 301, "h": 60}]}"#,
        );
        assert_eq!(notes(&report), [], "{window}");
        let q = visualized(skin, &[40]);
        let y = middle(&q);
        assert_eq!(
            top(&q, 100.0 + 110.5, y),
            Some(rgba(0x00ff_00ff)),
            "{window}"
        );
    }
}

/// C32: a judge width of 1 to 1 000 ms is kept and anything else drops the
/// visualizer with a note, the skin kept; likewise a hit-error width of 1
/// to 4 096 texels and a timing visualizer's of at least 1.
#[test]
fn a_visualizer_past_its_caps_drops_with_a_note() {
    let dropped = |field| vec![(Severity::Dropped, NoteKind::UnsupportedField { field })];
    let dst =
        |id: &str| format!(r#"{{"id": "{id}", "dst": [{{"x": 0, "y": 0, "w": 100, "h": 20}}]}}"#);
    for (table, fields, want) in [
        ("timingvisualizer", r#""judgeWidthMillis": 1000"#, vec![]),
        (
            "timingvisualizer",
            r#""judgeWidthMillis": 1001"#,
            dropped("judgeWidthMillis"),
        ),
        (
            "timingvisualizer",
            r#""judgeWidthMillis": 0"#,
            dropped("judgeWidthMillis"),
        ),
        ("timingvisualizer", r#""width": 0"#, dropped("width")),
        ("hiterrorvisualizer", r#""judgeWidthMillis": 1000"#, vec![]),
        (
            "hiterrorvisualizer",
            r#""judgeWidthMillis": 1001"#,
            dropped("judgeWidthMillis"),
        ),
        ("hiterrorvisualizer", r#""width": 4096"#, vec![]),
        ("hiterrorvisualizer", r#""width": 4097"#, dropped("width")),
        ("hiterrorvisualizer", r#""width": 0"#, dropped("width")),
    ] {
        let (_d, skin, report) = json(
            &format!(r#""{table}": [{{"id": "v", {fields}}}],"#),
            &dst("v"),
        );
        assert_eq!(notes(&report), want, "{table} {fields}");
        let drawn = !visualized(skin, &[40]).is_empty();
        assert_eq!(drawn, want.is_empty(), "{table} {fields}");
    }
}

/// A hit-error visualizer's average weight that is not finite is the
/// finite weight that steps alike: the skin imports with no note.
#[test]
fn an_infinite_average_weight_imports() {
    let (_d, skin, report) = json(
        r#""hiterrorvisualizer": [{"id": "he", "alpha": 1e999}],"#,
        r#"{"id": "he", "dst": [{"x": 100, "y": 800, "w": 301, "h": 60}]}"#,
    );
    assert_eq!(notes(&report), []);
    // The step of +40 saturates, `(long) (∞ · 40)`, to `Long.MAX_VALUE`;
    // the average's line reads its low 32 bits (`intValue`), −1, so it is
    // at column `150 + 1 = 151`.
    let q = visualized(skin, &[40]);
    let y = middle(&q);
    assert_eq!(top(&q, 100.0 + 151.5, y), Some(rgba(0xff00_00ff)));
}

/// 150 BPM, 300 from 1 s; notes at 0.1 to 0.5 s (five at 150, so MAIN)
/// and at 1.5 and 2 s.
fn tempo_chart() -> (Chart, ChartProfile) {
    let lane = |l| Lane::new(l).unwrap();
    let mut b = ChartBuilder::new(150_000).bpm(TimeUs(1_000 * MS), 300_000);
    for k in 1..=5 {
        b = b.tap(lane(1), TimeUs(100 * k * MS));
    }
    let mut chart = b
        .tap(lane(2), TimeUs(1_500 * MS))
        .tap(lane(3), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    chart.meta.length = ChartSummary::of(&chart).length;
    let profile = ChartProfile::of(&chart, &[]);
    (chart, profile)
}

/// The distinct colours of `quads`, sorted.
fn colours(quads: &[Quad]) -> Vec<[u8; 4]> {
    let mut c: Vec<[u8; 4]> = quads
        .iter()
        .map(|q| q.color.map(|c| (c * 255.0).round() as u8))
        .collect();
    c.sort_unstable();
    c.dedup();
    c
}

/// C31: a BPM graph's colours as the source reads them: the hex digits
/// wherever they are, the first six, always opaque (`#12ab34` main;
/// `ff000080` max, opaque `ff0000`); one to five digits keep the default
/// with a note (`ff00` for other). The rows are 150 (MAIN) and 300 (the
/// highest), so the strokes are main, max and the transition's default
/// `7f7f7f`. A line width far past the model's coordinates draws as the
/// source's, clamped, with no note.
#[test]
fn a_bpm_graph_reads_its_colours_as_the_source() {
    let tables = r##""bpmgraph": [{"id": "bg", "mainBPMColor": "#12ab34", "maxBPMColor": "ff000080", "otherBPMColor": "ff00"}],"##;
    let (_d, skin, report) = json(
        tables,
        r#"{"id": "bg", "dst": [{"x": 0, "y": 0, "w": 200, "h": 100}]}"#,
    );
    assert_eq!(
        notes(&report),
        [(
            Severity::Fallback,
            NoteKind::InvalidColour {
                field: "otherBPMColor"
            }
        )]
    );
    let (chart, profile) = tempo_chart();
    let main = Tempo::from_units(150 * Tempo::UNITS_PER_BPM).unwrap();
    let score = PlayScore {
        profile: Some(&profile),
        main: Some(main),
        ..PlayScore::NONE
    };
    let q = Drawn::over_score(skin, &chart, score).frame(0, &[]);
    assert_eq!(
        colours(&q),
        [rgba(0x12ab_34ff), rgba(0x7f7f_7fff), rgba(0xff00_00ff)]
    );
}

/// C32: a BPM graph's line width is kept up to 16 texels and drops the
/// graph with a note past it. At 16 over 100 rows the rows of 150 (MAIN)
/// and 300 sit at `⌊0.5 · 84⌋ = 42` and `⌊(log10 2 + log10 8)/(2 log10 8) ·
/// 84⌋ = 56`, 14 apart, less than the stroke, so no transition is drawn:
/// only the default MAIN `00ff00` and highest `ff0000`. At 17 nothing is
/// drawn, with the note.
#[test]
fn a_bpm_graph_keeps_line_widths_up_to_16() {
    let (chart, profile) = tempo_chart();
    let main = Tempo::from_units(150 * Tempo::UNITS_PER_BPM).unwrap();
    let score = PlayScore {
        profile: Some(&profile),
        main: Some(main),
        ..PlayScore::NONE
    };
    let dst = r#"{"id": "bg", "dst": [{"x": 0, "y": 0, "w": 200, "h": 100}]}"#;
    let (_d, skin, report) = json(r#""bpmgraph": [{"id": "bg", "lineWidth": 16}],"#, dst);
    assert_eq!(notes(&report), []);
    let q = Drawn::over_score(skin, &chart, score).frame(0, &[]);
    assert_eq!(colours(&q), [rgba(0x00ff_00ff), rgba(0xff00_00ff)]);
    let (_d, skin, report) = json(r#""bpmgraph": [{"id": "bg", "lineWidth": 17}],"#, dst);
    assert_eq!(
        notes(&report),
        [(
            Severity::Dropped,
            NoteKind::UnsupportedField { field: "lineWidth" }
        )]
    );
    assert_eq!(Drawn::over_score(skin, &chart, score).frame(0, &[]), []);
}

/// C32: a BPM graph whose stretch is not the default drops with a note:
/// the source draws its first frame's texture through the stretch with a
/// negative height. The default stretch (0) is kept.
#[test]
fn a_bpm_graph_drops_a_stretch_other_than_the_default() {
    let (_d, _skin, report) = json(
        r#""bpmgraph": [{"id": "bg"}],"#,
        r#"{"id": "bg", "stretch": 9, "dst": [{"x": 0, "y": 0, "w": 200, "h": 100}]}"#,
    );
    assert_eq!(notes(&report), [(Severity::Dropped, NoteKind::Stretch(9))]);
    let (_d, _skin, report) = json(
        r#""bpmgraph": [{"id": "bg"}],"#,
        r#"{"id": "bg", "stretch": 0, "dst": [{"x": 0, "y": 0, "w": 200, "h": 100}]}"#,
    );
    assert_eq!(notes(&report), []);
}

/// A JSON `null` colour, which fails the source's skin, is noted: a BPM
/// graph's keeps its default, a visualizer's draws red.
#[test]
fn a_null_colour_is_noted() {
    let (_d, _skin, report) = json(
        r#""bpmgraph": [{"id": "bg", "mainBPMColor": null}],"#,
        r#"{"id": "bg", "dst": [{"x": 0, "y": 0, "w": 200, "h": 100}]}"#,
    );
    assert_eq!(
        notes(&report),
        [(
            Severity::Fallback,
            NoteKind::InvalidColour {
                field: "mainBPMColor"
            }
        )]
    );
    let (_d, skin, report) = json(
        r#""timingvisualizer": [{"id": "tv", "PGColor": null, "PRColor": null}],"#,
        r#"{"id": "tv", "dst": [{"x": 100, "y": 1000, "w": 301, "h": 20}]}"#,
    );
    assert_eq!(
        notes(&report),
        [
            (
                Severity::Fallback,
                NoteKind::InvalidColour { field: "PGColor" }
            ),
            (
                Severity::Fallback,
                NoteKind::InvalidColour { field: "PRColor" }
            ),
        ]
    );
    let q = visualized(skin, &[40]);
    let y = middle(&q);
    assert_eq!(top(&q, 100.0 + 135.5, y), Some(rgba(0xff00_00ff)));
    assert_eq!(top(&q, 100.0 + 25.5, y), Some(rgba(0xff00_00ff)));
}

/// C31: a Lua string `alpha` is LuaJ's `tofloat()`: `"18446744073709551616"`
/// scans as a wrapping `long`, 0, so the average stays at 0, its line on
/// the centre's column 150 (drawn after the centre); `" 0.5 "` is 0.5, the
/// average `⌊0.5 · 40⌋ = 20`, its line at 130. The same JSON string is
/// `Float.parseFloat`, about 1.8e19: the step saturates to `Long.MAX_VALUE`,
/// whose low 32 bits are −1, the line at 151.
#[test]
fn a_lua_alpha_string_is_luaj_s_float() {
    let dst = "{ id = \"he\", dst = { { x = 100, y = 800, w = 301, h = 60 } } }";
    for (alpha, column) in [("18446744073709551616", 150.5), (" 0.5 ", 130.5)] {
        let (_d, skin, report) = lua(
            &format!("skin.hiterrorvisualizer = {{ {{ id = \"he\", alpha = \"{alpha}\" }} }}"),
            dst,
        );
        assert_eq!(notes(&report), [], "{alpha}");
        let q = visualized(skin, &[40]);
        assert_eq!(
            top(&q, 100.0 + column, middle(&q)),
            Some(rgba(0xff00_00ff)),
            "{alpha}"
        );
    }
    let (_d, skin, _) = json(
        r#""hiterrorvisualizer": [{"id": "he", "alpha": "18446744073709551616"}],"#,
        r#"{"id": "he", "dst": [{"x": 100, "y": 800, "w": 301, "h": 60}]}"#,
    );
    let q = visualized(skin, &[40]);
    assert_eq!(top(&q, 100.0 + 151.5, middle(&q)), Some(rgba(0xff00_00ff)));
}

/// C31: a JSON string `alpha` is Java's `Float.parseFloat`: `0.5f`,
/// `0x1p-1` and ` 0.5D ` are all 0.5, so the average after +40 ms is
/// `⌊0.5 · 40⌋ = 20`, its line at `150 − 20 = 130`; `0.5ff` is no float,
/// and the import refuses the field as the source's skin fails.
#[test]
fn a_json_alpha_string_is_java_s_float() {
    let dst = r#"{"id": "he", "dst": [{"x": 100, "y": 800, "w": 301, "h": 60}]}"#;
    for alpha in ["0.5f", "0x1p-1", " 0.5D "] {
        let (_d, skin, report) = json(
            &format!(r#""hiterrorvisualizer": [{{"id": "he", "alpha": "{alpha}"}}],"#),
            dst,
        );
        assert_eq!(notes(&report), [], "{alpha}");
        let q = visualized(skin, &[40]);
        assert_eq!(
            top(&q, 100.0 + 130.5, middle(&q)),
            Some(rgba(0xff00_00ff)),
            "{alpha}"
        );
    }
    let l = import_sized(
        [1_920, 1_080],
        &format!(
            r#""hiterrorvisualizer": [{{"id": "he", "alpha": "0.5ff"}}], "destination": [{dst}]"#
        ),
        &[],
    );
    assert!(
        matches!(
            l.err(),
            skin_beatoraja::ImportError::Type { field: "alpha", .. }
        ),
        "0.5ff"
    );
}

/// A BPM graph's `filter` is noted and ignored: the source draws a scaled
/// graph through its bilinear shader, which blends adjoining strokes; the
/// model keeps them hard. A graph sized by its first key at 100 × 100 and
/// drawn 200 wide from 1 s (its keys hold 200 until 2 s) shows only its strokes' own colours (MAIN
/// `00ff00`, the transition `7f7f7f`, the highest `ff0000`), none between
/// them, across the full 200 pixels.
#[test]
fn a_bpm_graph_notes_its_filter_and_keeps_hard_edges() {
    let (chart, profile) = tempo_chart();
    let main = Tempo::from_units(150 * Tempo::UNITS_PER_BPM).unwrap();
    let score = PlayScore {
        profile: Some(&profile),
        main: Some(main),
        ..PlayScore::NONE
    };
    let (_d, skin, report) = json(
        r#""bpmgraph": [{"id": "bg"}],"#,
        r#"{"id": "bg", "filter": 1, "dst": [
            {"time": 0, "x": 0, "y": 0, "w": 100, "h": 100},
            {"time": 1000, "x": 0, "y": 0, "w": 200, "h": 100},
            {"time": 2000, "x": 0, "y": 0, "w": 200, "h": 100}]}"#,
    );
    assert_eq!(
        notes(&report),
        [(Severity::Ignored, NoteKind::Ignored { field: "filter" })]
    );
    let q = Drawn::over_score(skin, &chart, score).frame(1_000, &[]);
    assert_eq!(
        colours(&q),
        [rgba(0x00ff_00ff), rgba(0x7f7f_7fff), rgba(0xff00_00ff)]
    );
    let right = q
        .iter()
        .map(|q| q.rect[0] + q.rect[2])
        .fold(f32::MIN, f32::max);
    let left = q.iter().map(|q| q.rect[0]).fold(f32::MAX, f32::min);
    assert!((right - left - 200.0).abs() < 1e-3, "{left} {right}");
}

/// C33: a timing visualizer following HIDDEN+ (offset 5) over lanes: with
/// HIDDEN+ on both its band and its lines draw; with HIDDEN+ off the source
/// sets its destination's alpha to 0, which hides the band, while the
/// lines, in their own colour and alpha, still draw: the line of +40 ms in
/// `00FF00` and no band colour.
#[test]
fn a_timing_visualizer_s_lines_outlast_hidden_plus() {
    let l = common::lanes::with(
        r#""timingvisualizer": [{"id": "tv"}]"#,
        &[r#"{"id": "tv", "offset": 5, "dst": [{"x": 500, "y": 1000, "w": 301, "h": 20}]}"#],
    );
    assert!(
        !l.report()
            .notes
            .iter()
            .any(|n| matches!(n.kind, NoteKind::RuntimeOffset(_)))
    );
    let frame = |enabled: bool| {
        let score = PlayScore {
            areas: Some(AREAS),
            ..PlayScore::NONE
        };
        let mut d = Drawn::over_score(l.ok().skin.clone(), &taps(), score);
        d.record(40);
        d.view.hidden = if enabled { 100 } else { 0 };
        let mut readouts = common::play::readouts_for(d.view);
        (readouts.covers.hidden, readouts.covers.hidden_enabled) = (100, enabled);
        d.readouts = Some(readouts);
        d.frame(2_000, &[])
    };
    let band = |q: &[Quad]| colours(q).contains(&rgba(0x0000_88ff));
    let line = |q: &[Quad]| colours(q).contains(&rgba(0x00ff_00ff));
    let on = frame(true);
    assert!(band(&on) && line(&on));
    let off = frame(false);
    assert!(
        !band(&off) && line(&off),
        "HIDDEN+ off: {:?}",
        colours(&off)
    );
}

/// C31: a notes graph is a `judgegraph` of type 0 on play, revealed over
/// its default 500 ms from the screen's start (READY, at 0 here): the
/// chart's notes are in its second second of four, so at 100 ms (a fifth
/// revealed) they are not drawn yet and at 1 s they are, as at 100 ms with
/// a `delay` of 0. Types 1 and 2 drop with a note.
#[test]
fn a_notes_graph_is_type_0_revealed_over_500_ms() {
    let dst = r#"{"id": "ng", "dst": [{"x": 0, "y": 0, "w": 300, "h": 100}]}"#;
    let chart = taps();
    let mut chart = chart;
    chart.meta.length = ChartSummary::of(&chart).length;
    let profile = ChartProfile::of(&chart, &[]);
    let score = PlayScore {
        profile: Some(&profile),
        ..PlayScore::NONE
    };
    let frame = |fields: &str, ms: i64| {
        let (_d, skin, report) = json(&format!(r#""judgegraph": [{{"id": "ng"{fields}}}],"#), dst);
        assert_eq!(notes(&report), [], "{fields}");
        Drawn::over_score(skin, &chart, score).frame(ms, &[])
    };
    // The chips are drawn from the chip grid, sampled `NearestBase`; the
    // background and the cursor from the white image.
    let chips = |q: Vec<Quad>| -> Vec<Quad> {
        q.into_iter()
            .filter(|q| q.filter == Filter::NearestBase)
            .collect()
    };
    let late = chips(frame("", 1_000));
    assert!(!late.is_empty());
    assert_eq!(chips(frame("", 100)), []);
    assert_eq!(chips(frame(r#", "delay": 0"#, 100)), late);
    for ty in [1, 2] {
        let (_d, _skin, report) = json(
            &format!(r#""judgegraph": [{{"id": "ng", "type": {ty}}}],"#),
            dst,
        );
        assert_eq!(
            notes(&report),
            [(
                Severity::Dropped,
                NoteKind::UnsupportedField { field: "type" }
            )],
            "{ty}"
        );
    }
}

/// C31: a timing visualizer's colours as the source reads them. A checked
/// colour must be hex digits only, so `#00ff00` draws red with a note
/// (`colorStringValidation`); the empty POOR colour is read by
/// `Color.valueOf` alone, so `#20304080` is `20304080`; with `transparent`
/// 1 that band is clear, whatever its colour.
#[test]
fn timing_visualizer_colours_follow_the_source_s_two_readings() {
    let dst = r#"{"id": "tv", "dst": [{"x": 100, "y": 1000, "w": 301, "h": 20}]}"#;
    let (_d, skin, report) = json(
        r##""timingvisualizer": [{"id": "tv", "lineColor": "#00ff00", "PRColor": "#20304080"}],"##,
        dst,
    );
    assert_eq!(
        notes(&report),
        [(
            Severity::Fallback,
            NoteKind::InvalidColour { field: "lineColor" }
        )]
    );
    let q = visualized(skin, &[40]);
    let y = middle(&q);
    assert_eq!(top(&q, 100.0 + 25.5, y), Some(rgba(0x2030_4080)));
    assert_eq!(top(&q, 100.0 + 190.5, y), Some(rgba(0xff00_00ff)));
    let (_d, skin, report) = json(
        r#""timingvisualizer": [{"id": "tv", "transparent": 1, "PRColor": "zz"}],"#,
        dst,
    );
    assert_eq!(notes(&report), []);
    let q = visualized(skin, &[40]);
    let y = middle(&q);
    assert_eq!(top(&q, 100.0 + 25.5, y), None);
    assert_eq!(top(&q, 100.0 + 35.5, y), Some(rgba(0x8800_00ff)));
}

/// C31: a hit-error visualizer's `hiterrorMode` 0 draws no line, and its
/// `alpha` is a float in JSON, as a string, and in Lua: 0.5 steps the
/// average by `⌊0.5 · 40⌋ = 20`, its line at `150 − 20 = 130`.
#[test]
fn hit_error_lines_and_weight_read_as_the_source() {
    let dst = r#"{"id": "he", "dst": [{"x": 100, "y": 800, "w": 301, "h": 60}]}"#;
    let (_d, skin, _) = json(
        r#""hiterrorvisualizer": [{"id": "he", "hiterrorMode": 0}],"#,
        dst,
    );
    let q = visualized(skin, &[40]);
    assert_eq!(top(&q, 100.0 + 110.5, middle(&q)), None);
    let weighted = |q: &[Quad]| top(q, 100.0 + 130.5, middle(q));
    for alpha in ["0.5", "\"0.5\""] {
        let (_d, skin, report) = json(
            &format!(r#""hiterrorvisualizer": [{{"id": "he", "alpha": {alpha}}}],"#),
            dst,
        );
        assert_eq!(notes(&report), [], "{alpha}");
        assert_eq!(
            weighted(&visualized(skin, &[40])),
            Some(rgba(0xff00_00ff)),
            "{alpha}"
        );
    }
    let (_d, skin, _) = lua(
        "skin.hiterrorvisualizer = { { id = \"he\", alpha = 0.5 } }",
        "{ id = \"he\", dst = { { x = 100, y = 800, w = 301, h = 60 } } }",
    );
    assert_eq!(weighted(&visualized(skin, &[40])), Some(rgba(0xff00_00ff)));
}

/// A stretch other than the default and a filter are noted and ignored: a
/// visualizer places its band, lines and pixmap in its rect.
#[test]
fn a_visualizer_notes_its_stretch_and_filter() {
    let (_d, _skin, report) = json(
        r#""hiterrorvisualizer": [{"id": "he"}],"#,
        r#"{"id": "he", "stretch": 1, "filter": 1, "dst": [{"x": 100, "y": 800, "w": 301, "h": 60}]}"#,
    );
    assert_eq!(
        notes(&report),
        [
            (Severity::Ignored, NoteKind::Ignored { field: "stretch" }),
            (Severity::Ignored, NoteKind::Ignored { field: "filter" }),
        ]
    );
}

/// A visualizer the failure presentation draws (on the failed timer, 3)
/// is critical: past its caps it refuses the import rather than drop.
#[test]
fn a_failure_visualizer_past_its_caps_refuses() {
    let l = import_sized(
        [1_920, 1_080],
        r#""timingvisualizer": [{"id": "tv", "judgeWidthMillis": 0}],
           "destination": [{"id": "tv", "timer": 3, "dst": [{"x": 0, "y": 0, "w": 100, "h": 20}]}]"#,
        &[],
    );
    assert!(matches!(
        l.err(),
        skin_beatoraja::ImportError::Critical {
            why: NoteKind::UnsupportedField {
                field: "judgeWidthMillis"
            },
            ..
        }
    ));
}

/// C31: a timing visualizer's line width is clamped to 1 to 4 as the
/// source clamps it, with no note: 6 is 4, so the line of +40 ms covers
/// `x0 + (301 − 4)/2 + 40 = x0 + 188.5` to `x0 + 192.5`, unrounded.
#[test]
fn a_timing_visualizer_clamps_its_line_width() {
    let (_d, skin, report) = json(
        r#""timingvisualizer": [{"id": "tv", "lineWidth": 6}],"#,
        r#"{"id": "tv", "dst": [{"x": 100, "y": 1000, "w": 301, "h": 20}]}"#,
    );
    assert_eq!(notes(&report), []);
    let q = visualized(skin, &[40]);
    let y = middle(&q);
    let green = Some(rgba(0x00ff_00ff));
    assert_eq!(top(&q, 100.0 + 188.6, y), green);
    assert_eq!(top(&q, 100.0 + 192.4, y), green);
    assert_ne!(top(&q, 100.0 + 192.6, y), green);
    assert_ne!(top(&q, 100.0 + 188.4, y), green);
}

/// C33: a graph or visualizer whose destination lists LIFT, SUDDEN+ or
/// HIDDEN+ (offsets 3 to 5) moves with it, as every object the source
/// draws does (one real skin's hit-error visualizer lists offset 3): it is
/// kept, with no note, and draws.
#[test]
fn graphs_and_visualizers_follow_the_covers() {
    for (table, id) in [
        ("hiterrorvisualizer", "v"),
        ("timingvisualizer", "v"),
        ("bpmgraph", "v"),
        ("judgegraph", "v"),
    ] {
        let (_d, skin, report) = json(
            &format!(r#""{table}": [{{"id": "{id}"}}],"#),
            r#"{"id": "v", "offsets": [3, 4, 5], "dst": [{"x": 100, "y": 800, "w": 301, "h": 60}]}"#,
        );
        assert!(
            !report
                .notes
                .iter()
                .any(|n| matches!(n.kind, NoteKind::RuntimeOffset(_))),
            "{table}: {:?}",
            notes(&report)
        );
        assert_eq!(skin.kind(), skin_model::ScreenKind::Play, "{table}");
        if table.ends_with("visualizer") {
            assert!(!visualized(skin, &[40]).is_empty(), "{table}");
        }
    }
}
