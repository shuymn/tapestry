//! Number 92, the main BPM, on play (docs/stage3.md "Play readouts"): the
//! attempt's whole-chart MAIN tempo in whole BPM, truncated as the
//! source's `(int)` cast (`skin/property/IntegerPropertyFactory.java:167-173`),
//! drawn while it is known and never while loading. JSON by id and name,
//! and Lua's `number(92)`.

mod common;

use bms_model::{ChartBuilder, Lane, Tempo, TimeUs};
use common::play::{Drawn, Loader, round};
use common::{import_body, write};
use skin_beatoraja::lua::Disposition;
use skin_beatoraja::{Cancel, Config, DeviationId, LuaImported, NoteKind, read_lua_header};
use skin_model::BpmRange;

const MS: i64 = 1_000;

/// A chart of 10 notes at 120 BPM, then 11 at 180 BPM.
fn chart() -> bms_model::Chart {
    let b = (0..10).fold(ChartBuilder::new(120_000), |b, i| {
        b.tap(Lane::new(1).unwrap(), TimeUs((1_000 + 100 * i) * MS))
    });
    (0..11)
        .fold(b.bpm(TimeUs(5_000 * MS), 180_000), |b, i| {
            b.tap(Lane::new(2).unwrap(), TimeUs((6_000 + 100 * i) * MS))
        })
        .build()
        .unwrap()
}

/// The chart's range, 120 to 200 BPM: its maximum is neither MAIN nor the
/// current tempo.
const RANGE: BpmRange = BpmRange {
    min_x100: 12_000,
    max_x100: 20_000,
};

fn tempo(milli: i64) -> Option<Tempo> {
    Tempo::from_milli(milli)
}

/// An EX score of one cell at y = 0, then number `prop` of four cells at
/// y = 50.
fn main_number(prop: &str) -> common::Loaded {
    import_body(&format!(
        r#""value": [
             {{"id": "ex", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 1, "ref": 71}},
             {{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4, {prop}}}],
           "destination": [
             {{"id": "ex", "dst": [{{"x": 0, "y": 0, "w": 12, "h": 20}}]}},
             {{"id": "v", "dst": [{{"x": 0, "y": 50, "w": 12, "h": 20}}]}}]"#
    ))
}

/// The digits of the number at y = 50 (1 010 on the canvas, which counts
/// from the top) among `quads`, read through `glyph`.
fn digits_of(
    glyph: &std::collections::HashMap<[u32; 4], char>,
    quads: &[skin_model::Quad],
) -> String {
    let mut cells: Vec<_> = quads
        .iter()
        .filter(|q| round(q.rect)[1] == 1_010)
        .map(|q| (round(q.rect)[0], glyph[&q.uv.map(f32::to_bits)]))
        .collect();
    cells.sort_unstable();
    cells.into_iter().map(|(_, c)| c).collect()
}

/// C6: number 92 shows the whole chart's MAIN in whole BPM, truncated
/// (150.7 shows 150, where rounding shows 151), not the chart's maximum or
/// its current tempo; nothing without a MAIN tempo; nothing while loading.
/// It is a play readout, noted D4 (G5: MAIN is tapestry's deterministic
/// one).
#[test]
fn number_92_shows_the_whole_charts_main_truncated() {
    for prop in [r#""ref": 92"#, r#""value": "mainbpm""#] {
        let l = main_number(prop);
        let notes = &l.report().notes;
        assert!(
            notes
                .iter()
                .all(|n| n.kind == NoteKind::Deviation(DeviationId::D4)),
            "{prop}: {notes:?}"
        );
        assert_eq!(notes.len(), 1, "{prop}: {notes:?}");
        // The skin's files stay with `l` while its plays build atlases.
        let skin = l.ok().skin.clone();
        let glyph = Drawn::new(skin.clone(), 100).ex_glyphs(1_000);
        for (main, want) in [
            (tempo(180_000), "180"),
            (tempo(150_700), "150"),
            (tempo(150_000), "150"),
            (tempo(1_234_000), "1234"),
            (None, ""),
        ] {
            let mut d = Drawn::over_main(skin.clone(), &chart(), main, Some(RANGE));
            for ms in [1_200, 7_000] {
                assert_eq!(
                    digits_of(&glyph, &d.frame(ms, &[])),
                    want,
                    "{prop} {main:?} {ms}"
                );
            }
        }
        // Unavailable while loading: no library main tempo yet.
        let mut loader: Loader = main_number(prop).loading();
        assert!(digits_of(&glyph, &loader.frame(100)).is_empty(), "{prop}");
    }
}

/// A Lua play skin of image `i` under each `draw` callback body of
/// `draws`, destination `k` drawn `10 + k` wide.
fn lua_skin(draws: &[&str]) -> (tempfile::TempDir, LuaImported) {
    let d = tempfile::tempdir().unwrap();
    let dsts: Vec<String> = draws
        .iter()
        .enumerate()
        .map(|(k, draw)| {
            format!(
                "{{ id = \"i\", draw = function() {draw} end, dst = {{ {{ w = {}, h = 10 }} }} }}",
                10 + k
            )
        })
        .collect();
    let lua = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 8, h = 8 }} }}\n\
         skin.destination = {{ {} }}\n\
         return skin\n",
        dsts.join(",\n")
    );
    write(d.path(), "s.luaskin", lua.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    let h = read_lua_header(d.path(), "s.luaskin", &Cancel::new()).unwrap();
    let i = common::lua_play(&h, &Config::default(), &Cancel::new()).unwrap();
    (d, i)
}

fn widths(rects: &[[i32; 4]]) -> Vec<i32> {
    let mut w: Vec<i32> = rects.iter().map(|r| r[2]).collect();
    w.sort_unstable();
    w
}

/// C6: a Lua play callback's `number(92)` lowers and reads the truncated
/// main BPM: 150 for a 150.7 MAIN, never 151, the maximum 200 or the
/// current 120; under no MAIN tempo the read is unavailable and nothing
/// that reads it draws.
#[test]
fn a_lua_callback_reads_number_92() {
    let (_d, i) = lua_skin(&[
        "return ms.number(92) == 150",
        "return ms.number(92) == 151",
        "return ms.number(92) == 200",
        "return ms.number(92) == 120",
        "return ms.number(92) >= 0",
    ]);
    assert_eq!(i.callbacks, [Disposition::Lowered; 5]);
    let skin = i.imported.skin;
    let mut d = Drawn::over_main(skin.clone(), &chart(), tempo(150_700), Some(RANGE));
    d.readouts = Some(skin_model::FrameReadouts {
        bpm_current_x100: Some(12_000),
        ..common::play::readouts_for(d.view)
    });
    assert_eq!(widths(&d.rects(1_200)), [10, 14]);
    let mut d = Drawn::over_main(skin, &chart(), None, Some(RANGE));
    assert_eq!(widths(&d.rects(1_200)), Vec::<i32>::new());
}
