//! The load's progress, the play's progress and the chart's length on a
//! play skin (docs/stage3.md R3, "Play readouts"): number 165 reads
//! `load.percent` (`skin/property/IntegerPropertyFactory.java:224-229`),
//! graph 102 fills `load.percent / 100` and graph 101
//! `time.progress_ppm / 10⁶` (`skin/property/FloatPropertyFactory.java:
//! 128-137, 284-290`, `skin/SkinGraph.java:98-106`), and numbers 1163 and
//! 1164 read the chart's 1× length's whole minutes and seconds within
//! the minute (`IntegerPropertyFactory.java:473-478`), the same at
//! every practice speed and while loading from the chosen chart's library
//! length. JSON by id and name, and Lua.

mod common;

use std::collections::HashMap;

use bms_model::{Chart, ChartBuilder, Lane, Speed, TimeUs};
use common::play::{Drawn, Loader, MS, context, round};
use common::{import_body, write};
use skin_beatoraja::lua::Disposition;
use skin_beatoraja::{Cancel, Config, DeviationId, NoteKind, read_lua_header};
use skin_model::{Attempt, PlayContext, Quad};

/// The EX score (one cell at y 0, the lowest on the canvas, for the
/// glyphs), number 165 (three cells at y 50), 1163 and 1164 (two cells at
/// y 100 and 150), graph 102 across (200 × 20 at y 300) and up (20 × 200
/// at x 500, y 400), and graph 101 across (200 × 20 at y 340); `refs`
/// names the three numbers' properties.
fn skin_with(refs: [&str; 3]) -> common::Loaded {
    let [load, min, sec] = refs;
    import_body(&format!(
        r#""value": [
             {{"id": "ex", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 1, "ref": 71}},
             {{"id": "p", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 3, {load}}},
             {{"id": "m", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 2, {min}}},
             {{"id": "s", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 2, {sec}}}],
           "graph": [
             {{"id": "la", "src": "a", "w": 64, "h": 64, "type": 102, "angle": 0}},
             {{"id": "lu", "src": "a", "w": 64, "h": 64, "type": 102, "angle": 1}},
             {{"id": "sa", "src": "a", "w": 64, "h": 64, "type": 101, "angle": 0}}],
           "destination": [
             {{"id": "ex", "dst": [{{"x": 0, "y": 0, "w": 12, "h": 20}}]}},
             {{"id": "p", "dst": [{{"x": 0, "y": 50, "w": 12, "h": 20}}]}},
             {{"id": "m", "dst": [{{"x": 0, "y": 100, "w": 12, "h": 20}}]}},
             {{"id": "s", "dst": [{{"x": 0, "y": 150, "w": 12, "h": 20}}]}},
             {{"id": "la", "dst": [{{"x": 100, "y": 300, "w": 200, "h": 20}}]}},
             {{"id": "lu", "dst": [{{"x": 500, "y": 400, "w": 20, "h": 200}}]}},
             {{"id": "sa", "dst": [{{"x": 100, "y": 340, "w": 200, "h": 20}}]}}]"#
    ))
}

const BY_ID: [&str; 3] = [r#""ref": 165"#, r#""ref": 1163"#, r#""ref": 1164"#];
const BY_NAME: [&str; 3] = [
    r#""value": "loading_progress""#,
    r#""value": "chartlength_minute""#,
    r#""value": "chartlength_second""#,
];

/// The canvas rows (y down) of the three numbers and the bars.
const ROW_LOAD: i32 = 1_010;
const ROW_MIN: i32 = 960;
const ROW_SEC: i32 = 910;
/// Graph 102 across: `[100, 760, 200·f, 20]`; up: `[500, 480 + 200(1 − f),
/// 20, 200·f]`; graph 101 across: `[100, 720, 200·f, 20]`.
const TOP_LOAD_ACROSS: i32 = 760;
const TOP_SONG_ACROSS: i32 = 720;

/// What a frame shows.
#[derive(Debug, PartialEq)]
struct Shown {
    load: String,
    min: String,
    sec: String,
    /// Graph 102 across, graph 102 up and graph 101 across: each one's
    /// rect, rounded, and its uv's width or height over the image's
    /// full one, in thousandths.
    bars: [Option<([i32; 4], i32)>; 3],
}

fn digits(glyph: &HashMap<[u32; 4], char>, quads: &[Quad], row: i32) -> String {
    let mut cells: Vec<_> = quads
        .iter()
        .filter(|q| round(q.rect)[1] == row && round(q.rect)[3] == 20 && round(q.rect)[2] == 12)
        .map(|q| (round(q.rect)[0], glyph[&q.uv.map(f32::to_bits)]))
        .collect();
    cells.sort_unstable();
    cells.into_iter().map(|(_, c)| c).collect()
}

/// The full uv of the bars' image: its width and height.
#[derive(Clone, Copy)]
struct Full([f32; 2]);

fn shown(glyph: &HashMap<[u32; 4], char>, full: Full, quads: &[Quad]) -> Shown {
    let bar = |pick: &dyn Fn(&[i32; 4]) -> bool, vertical: bool| {
        let q = quads.iter().find(|q| pick(&round(q.rect)))?;
        let (uv, whole) = if vertical {
            (q.uv[3] - q.uv[1], full.0[1])
        } else {
            (q.uv[2] - q.uv[0], full.0[0])
        };
        #[expect(clippy::cast_possible_truncation, reason = "a thousandth of 0 to 1")]
        let k = (uv / whole * 1_000.0).round() as i32;
        Some((round(q.rect), k))
    };
    Shown {
        load: digits(glyph, quads, ROW_LOAD),
        min: digits(glyph, quads, ROW_MIN),
        sec: digits(glyph, quads, ROW_SEC),
        bars: [
            bar(&|r| r[0] == 100 && r[1] == TOP_LOAD_ACROSS, false),
            bar(&|r| r[0] == 500 && r[2] == 20, true),
            bar(&|r| r[0] == 100 && r[1] == TOP_SONG_ACROSS, false),
        ],
    }
}

/// One tap at 2 s, the chart's last object, its 1× length `length`.
fn chart(length: TimeUs) -> Chart {
    let mut c = ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    c.meta.length = length;
    c
}

/// The glyphs and the bars' full uv, read from a play frame at the end
/// (both bars full).
fn calibrate(skin: &skin_model::Skin) -> (HashMap<[u32; 4], char>, Full) {
    let mut d = Drawn::over(skin.clone(), &chart(TimeUs(0)));
    let glyph = d.ex_glyphs(3_000);
    let q = d.frame(3_000, &[]);
    let r = q
        .iter()
        .find(|q| round(q.rect) == [100, TOP_LOAD_ACROSS, 200, 20])
        .expect("graph 102 full in a play");
    (glyph, Full([r.uv[2] - r.uv[0], r.uv[3] - r.uv[1]]))
}

/// C14, C15: each number and graph is kept with one D4 note (the load's
/// curve, the progress to the last object, the length from notes and
/// background sounds) and nothing else.
#[test]
fn the_readouts_and_graphs_import_with_a_d4_note() {
    for refs in [BY_ID, BY_NAME] {
        let l = skin_with(refs);
        let notes: Vec<NoteKind> = l.report().notes.iter().map(|n| n.kind).collect();
        assert_eq!(notes, [NoteKind::Deviation(DeviationId::D4); 6], "{refs:?}");
    }
}

/// C14: in a play, 165 reads 100 and graph 102 is full both ways; graph
/// 101 fills `time.progress_ppm / 10⁶` across: 0 wide in READY, 250 000 ppm a
/// quarter (50 of 200 px), 1 at and after the last object.
#[test]
fn a_play_reads_the_load_full_and_the_progress() {
    for refs in [BY_ID, BY_NAME] {
        let l = skin_with(refs);
        let skin = l.ok().skin.clone();
        let (glyph, full) = calibrate(&skin);
        let mut d = Drawn::over(skin.clone(), &chart(TimeUs(125_400 * MS)));
        let ready = PlayContext {
            attempt: Attempt::Full {
                lead_in: TimeUs(1_000 * MS),
            },
            ..context()
        };
        d.play = skin.prepare(&d.chart, &ready).unwrap();
        let load = (
            Some(([100, TOP_LOAD_ACROSS, 200, 20], 1_000)),
            Some(([500, 480, 20, 200], 1_000)),
        );
        for (ms, song) in [
            (-500, Some(([100, TOP_SONG_ACROSS, 0, 20], 0))),
            (500, Some(([100, TOP_SONG_ACROSS, 50, 20], 250))),
            (1_000, Some(([100, TOP_SONG_ACROSS, 100, 20], 500))),
            (2_000, Some(([100, TOP_SONG_ACROSS, 200, 20], 1_000))),
            (9_000, Some(([100, TOP_SONG_ACROSS, 200, 20], 1_000))),
        ] {
            let s = shown(&glyph, full, &d.frame(ms, &[]));
            assert_eq!(s.load, "100", "{refs:?} {ms}");
            assert_eq!((s.bars[0], s.bars[1]), load, "{refs:?} {ms}");
            assert_eq!(s.bars[2], song, "{refs:?} {ms}");
        }
    }
}

/// C14: while loading at 45 %, 165 reads 45, graph 102 fills 0.45 of its
/// width (90 of 200 px, from the left) and of its height (90 px, from the
/// bottom), each cutting its uv alike; graph 101 is 0 wide, as in READY.
#[test]
fn loading_reads_the_loads_percent() {
    for refs in [BY_ID, BY_NAME] {
        let l = skin_with(refs);
        let skin = l.ok().skin.clone();
        let (glyph, full) = calibrate(&skin);
        let mut loader = Loader::new(skin);
        for (percent, want) in [(45, "45"), (7, "7"), (100, "100")] {
            loader.percent = percent;
            let s = shown(&glyph, full, &loader.frame(100));
            let px = 2 * percent;
            assert_eq!(s.load, want, "{refs:?}");
            assert_eq!(
                s.bars,
                [
                    Some(([100, TOP_LOAD_ACROSS, px, 20], 10 * percent)),
                    Some(([500, 680 - px, 20, px], 10 * percent)),
                    Some(([100, TOP_SONG_ACROSS, 0, 20], 0)),
                ],
                "{refs:?} {percent}"
            );
        }
    }
}

/// C15: 1163 and 1164 are `⌊L/60 000⌋` and `⌊L/1 000⌋ % 60` of the
/// chart's 1× length L in ms: 125 400 → 2, 5; 3 690 000 (an hour, a
/// minute and a half) → 61, 30; 0 → 0, 0.
/// The same while loading from the frame's chosen length; nothing while
/// it gives none.
#[test]
fn the_length_reads_whole_minutes_and_seconds() {
    for refs in [BY_ID, BY_NAME] {
        let l = skin_with(refs);
        let skin = l.ok().skin.clone();
        let (glyph, full) = calibrate(&skin);
        let mut loader = Loader::new(skin.clone());
        for (ms, min, sec) in [
            (125_400, "2", "5"),
            (3_690_000, "61", "30"),
            (59_999, "0", "59"),
            (0, "0", "0"),
        ] {
            let length = TimeUs(ms * MS);
            let mut d = Drawn::over(skin.clone(), &chart(length));
            for at in [0, 1_000, 9_000] {
                let s = shown(&glyph, full, &d.frame(at, &[]));
                assert_eq!(
                    (s.min.as_str(), s.sec.as_str()),
                    (min, sec),
                    "{refs:?} {ms} {at}"
                );
            }
            loader.length = Some(length);
            let s = shown(&glyph, full, &loader.frame(100));
            assert_eq!(
                (s.min.as_str(), s.sec.as_str()),
                (min, sec),
                "{refs:?} {ms} loading"
            );
        }
        loader.length = None;
        let s = shown(&glyph, full, &loader.frame(100));
        assert_eq!((s.min.as_str(), s.sec.as_str()), ("", ""), "{refs:?} none");
    }
}

/// A chart of 1× length 125.4 s at 120 BPM: a tap in measure 1 and one
/// at 0.7 of measure 62 (124 s + 1.4 s).
const LENGTH_BMS: &str = "#BPM 120\n#WAV01 a.wav\n#00111:01\n#06211:00000000000000010000\n";

/// C15, continuity: one chart of 1× length 125 400 ms, loaded at half
/// speed (k = 10), reads 2, 5 on its loading frame (the length its
/// library summary gives), on its READY frame and in two practice rounds,
/// where the loaded chart's own summary, in real times at that speed,
/// gives 250 800 ms: 4, 10.
#[test]
fn the_length_is_the_1x_charts_from_loading_through_practice() {
    let l = skin_with(BY_ID);
    let skin = l.ok().skin.clone();
    let (glyph, full) = calibrate(&skin);
    let summary =
        bms_parser::summarize(LENGTH_BMS.as_bytes(), &bms_parser::Limits::default()).unwrap();
    assert_eq!(summary.length, TimeUs(125_400 * MS));
    let dir = tempfile::tempdir().unwrap();
    let half = Speed::from_twentieths(10).unwrap();
    let opts = bms_parser::LoadOptions {
        speed: half,
        ..bms_parser::LoadOptions::default()
    };
    let chart = bms_parser::load_bms(LENGTH_BMS.as_bytes(), dir.path(), &opts)
        .unwrap()
        .chart;
    assert_eq!(chart.meta.length, TimeUs(125_400 * MS));
    assert_eq!(
        bms_model::ChartSummary::of(&chart).length,
        TimeUs(250_800 * MS),
        "the playback length differs"
    );
    let two_five = ("2".to_owned(), "5".to_owned());
    let mut loader = Loader::new(skin.clone());
    loader.length = Some(summary.length);
    let s = shown(&glyph, full, &loader.frame(100));
    assert_eq!((s.min, s.sec), two_five, "loading");
    let full_play = PlayContext {
        attempt: Attempt::Full {
            lead_in: TimeUs(1_000 * MS),
        },
        score: skin_model::PlayScore {
            speed: half,
            ..skin_model::PlayScore::NONE
        },
        ..context()
    };
    let mut d = Drawn::over_context(skin.clone(), &chart, &full_play);
    let s = shown(&glyph, full, &d.frame(-500, &[]));
    assert_eq!((s.min, s.sec), two_five.clone(), "READY");
    for (round, (start, end)) in [(1, (2_000, 6_000)), (2, (100_000, 120_000))] {
        let section = eval::Section {
            start: TimeUs(start * MS),
            end: TimeUs(end * MS),
            lead_in: TimeUs(1_000 * MS),
            initial_gauges: vec![common::play::GAUGES[0].start],
        };
        let ctx = PlayContext {
            attempt: Attempt::Section(&section),
            ..full_play
        };
        let mut d = Drawn::over_context(skin.clone(), &chart, &ctx);
        for at in [start - 500, start + 500] {
            let s = shown(&glyph, full, &d.frame(at, &[]));
            assert_eq!((s.min, s.sec), two_five.clone(), "round {round} at {at}");
        }
    }
}

/// A Lua play skin: graphs of type 102 and 101 by their `type`, number
/// `ref` 165, 1163 and 1164, and callbacks reading `number(165)`,
/// `number(1163)` and `number(1164)`, each image `w` its destination's
/// width.
fn lua_skin() -> (tempfile::TempDir, skin_beatoraja::LuaImported) {
    let d = tempfile::tempdir().unwrap();
    let lua = "local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = { { id = \"a\", path = \"a.png\" } }\n\
         skin.image = { { id = \"i\", src = \"a\", x = 0, y = 0, w = 8, h = 8 } }\n\
         skin.graph = {\n\
           { id = \"la\", src = \"a\", x = 0, y = 0, w = 64, h = 64, type = 102, angle = 0 },\n\
           { id = \"sa\", src = \"a\", x = 0, y = 0, w = 64, h = 64, type = 101, angle = 0 } }\n\
         skin.destination = {\n\
           { id = \"la\", dst = { { x = 100, y = 300, w = 200, h = 20 } } },\n\
           { id = \"sa\", dst = { { x = 100, y = 340, w = 200, h = 20 } } },\n\
           { id = \"i\", draw = function() return ms.number(165) == 45 end, dst = { { y = 500, w = 11, h = 10 } } },\n\
           { id = \"i\", draw = function() return ms.number(1163) == 2 end, dst = { { y = 500, w = 12, h = 10 } } },\n\
           { id = \"i\", draw = function() return ms.number(1164) == 5 end, dst = { { y = 500, w = 13, h = 10 } } },\n\
           { id = \"i\", draw = function() return ms.number(1163) == 4 end, dst = { { y = 500, w = 14, h = 10 } } } }\n\
         return skin\n";
    write(d.path(), "s.luaskin", lua.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    let h = read_lua_header(d.path(), "s.luaskin", &Cancel::new()).unwrap();
    let i = common::lua_play(&h, &Config::default(), &Cancel::new()).unwrap();
    (d, i)
}

/// C14, C15 through Lua: graph 102 at 45 % while loading is 90 px wide,
/// graph 101 a quarter of the way is 50 px; the callbacks read 45 while
/// loading and 2, 5 for a length of 125 400 ms (never 4).
#[test]
fn a_lua_skin_reads_the_progresses_and_the_length() {
    let (_d, i) = lua_skin();
    assert_eq!(i.callbacks, [Disposition::Lowered; 4]);
    let skin = i.imported.skin;
    let mut loader = Loader::new(skin.clone());
    loader.percent = 45;
    loader.length = Some(TimeUs(125_400 * MS));
    let mut widths: Vec<i32> = loader.rects(100).iter().map(|r| r[2]).collect();
    widths.sort_unstable();
    // Graph 101 reads 0 while loading.
    assert!(widths.contains(&90), "{widths:?}");
    assert!(!widths.contains(&200), "{widths:?}");
    assert_eq!(
        widths.iter().filter(|&&w| (11..=14).contains(&w)).count(),
        3,
        "{widths:?}"
    );
    assert!(widths.contains(&11) && widths.contains(&12) && widths.contains(&13));
    let mut d = Drawn::over(skin, &chart(TimeUs(125_400 * MS)));
    let mut widths: Vec<i32> = d.rects(500).iter().map(|r| r[2]).collect();
    widths.sort_unstable();
    // In a play: 102 full, 101 a quarter, 165 reads 100.
    assert_eq!(widths, [12, 13, 50, 200]);
}
