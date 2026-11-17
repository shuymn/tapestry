//! The BGA setting (design.md "Differences from the source", the BGA
//! setting): options 40 (`bgaoff`) and 41 (`bgaon`) answer from
//! `player.toml`'s `[bga] mode` and the session's autoplay on every
//! screen, by number, sign and name, JSON and Lua alike, with no note;
//! option 171 (`song_bga`) reads the chart's BGA flag on play, beside 170.

mod common;

use bms_model::ChartBuilder;
use common::play::Drawn;
use common::{import_with, skin_with};
use skin_beatoraja::BgaMode::{Autoplay, Off, On};
use skin_beatoraja::{
    Bga, BgaExpand, Cancel, Config, Imported, NoteKind, read_header, read_lua_header,
};
use skin_model::{ChartMetadata, ScreenKind};

/// Each setting the tests run under, and whether it shows the BGA: `on`
/// does, `off` does not even under autoplay, and `autoplay` does only in
/// an autoplay session.
const SETTINGS: [(Bga, bool); 4] = [
    (
        Bga {
            mode: On,
            autoplay: false,
            expand: BgaExpand::Full,
        },
        true,
    ),
    (
        Bga {
            mode: Off,
            autoplay: true,
            expand: BgaExpand::Full,
        },
        false,
    ),
    (
        Bga {
            mode: Autoplay,
            autoplay: true,
            expand: BgaExpand::Full,
        },
        true,
    ),
    (
        Bga {
            mode: Autoplay,
            autoplay: false,
            expand: BgaExpand::Full,
        },
        false,
    ),
];

/// A 1920×1080 play skin with source `a` (64×64) and image `i`, and
/// `rest` after them.
fn play_json(rest: &str) -> String {
    format!(
        "{{\"type\": 0, \"w\": 1920, \"h\": 1080,\n\
         \"source\": [{{\"id\": \"a\", \"path\": \"a.png\"}}],\n\
         \"image\": [{{\"id\": \"i\", \"src\": \"a\", \"w\": 64, \"h\": 64}}],\n\
         {rest}}}"
    )
}

/// `json` imported on play under `bga`, with the folder its atlas reads.
fn imported_in(json: &str, bga: Bga) -> (tempfile::TempDir, Imported) {
    let d = skin_with(json, &[("a.png", 64, 64)]);
    let i = import_with(d.path(), |c| c.bga = bga).unwrap_or_else(|e| panic!("{e:?}"));
    (d, i)
}

/// The widths drawn at 0 ms over a chart whose BGA flag is `bga`.
fn widths(skin: skin_model::Skin, bga: Option<bool>) -> Vec<i32> {
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let meta = ChartMetadata {
        bga,
        ..ChartMetadata::UNKNOWN
    };
    let mut d = Drawn::over_meta(skin, &chart, meta);
    let mut w: Vec<i32> = d.rects(0).iter().map(|r| r[2]).collect();
    w.sort_unstable();
    w
}

/// A destination of `i` under `op`, drawn `w` wide.
fn under(op: &str, w: i32) -> String {
    format!("{{\"id\": \"i\", \"op\": [{op}], \"dst\": [{{\"w\": {w}, \"h\": 10}}]}}")
}

/// The play destinations of [`options_40_41_follow_mode`]: each option by
/// number, sign or name, and its width.
const PLAY_OPS: [(&str, i32); 10] = [
    ("41", 10),
    ("40", 11),
    ("-40", 12),
    ("-41", 13),
    ("\"bgaon\"", 14),
    ("\"!bgaon\"", 15),
    ("\"bgaoff\"", 16),
    ("171", 17),
    ("170", 18),
    ("-171", 19),
];

/// Options 40 and 41 follow the setting alone on play, whatever the
/// chart's BGA flag: with the BGA shown 41 holds and 40 does not, hidden
/// the reverse. 171 holds only on a chart with a BGA and 170 only on a
/// chart known without one, whatever the setting; on a chart whose flag is
/// unknown neither does. The decision and result screens answer 40 and 41
/// alike.
#[test]
fn options_40_41_follow_mode() {
    let dsts: Vec<String> = PLAY_OPS.iter().map(|&(op, w)| under(op, w)).collect();
    let json = play_json(&format!("\"destination\": [{}]", dsts.join(",")));
    for (bga, shown) in SETTINGS {
        let at = format!("{:?} {}", bga.mode, bga.autoplay);
        let setting: &[i32] = if shown {
            &[10, 12, 14]
        } else {
            &[11, 13, 15, 16]
        };
        for (flag, chart) in [
            (Some(true), &[17][..]),
            (Some(false), &[18, 19][..]),
            (None, &[19][..]),
        ] {
            let (_d, i) = imported_in(&json, bga);
            let mut want = [setting, chart].concat();
            want.sort_unstable();
            assert_eq!(widths(i.skin, flag), want, "{at}, chart BGA {flag:?}");
        }
        assert_eq!(decision_tops(bga), decision_want(shown), "decision {at}");
        assert_eq!(result_drawn(bga), result_want(shown), "result {at}");
    }
}

/// The decision and result fixtures' options: each a selection fixture
/// image, drawn at its own row.
const SCREEN_OPS: [(&str, &str, i32); 4] = [
    ("40", "lamp", 100),
    ("-40", "lab", 200),
    ("41", "bar", 300),
    ("-41", "b0", 400),
];

/// The tops a decision skin under `bga` draws [`SCREEN_OPS`] at.
fn decision_tops(bga: Bga) -> Vec<f32> {
    let dsts: Vec<String> = SCREEN_OPS
        .iter()
        .map(|(op, id, y)| {
            format!(
                r#"{{"id": "{id}", "op": [{op}], "dst": [{{"x": 0, "y": {y}, "w": 8, "h": 8}}]}}"#
            )
        })
        .collect();
    let d = common::decision::folder(&format!(r#""destination": [{}]"#, dsts.join(",")));
    let (mut def, h) = read_header(d.path(), "s.json").unwrap();
    let config = Config {
        bga,
        ..Config::defaults(&h)
    };
    let i = skin_beatoraja::import(&mut def, &h, &config, ScreenKind::Decision)
        .unwrap_or_else(|e| panic!("{e:?}"));
    let mut tops = common::decision::Dec::new(i.skin).tops(0);
    tops.sort_by(f32::total_cmp);
    tops
}

/// The tops of [`SCREEN_OPS`] whose option holds with the BGA `shown`.
fn decision_want(shown: bool) -> Vec<f32> {
    let mut want: Vec<f32> = SCREEN_OPS
        .iter()
        .filter(|(op, ..)| matches!(*op, "41" | "-40") == shown)
        .map(|(.., y)| (1_080 - y - 8) as f32)
        .collect();
    want.sort_by(f32::total_cmp);
    want
}

/// The images of [`SCREEN_OPS`] a result skin under `bga` draws.
fn result_drawn(bga: Bga) -> Vec<[u8; 2]> {
    use common::result::{Res, best, folder, graph_json, ready};
    let dsts: Vec<String> = SCREEN_OPS
        .iter()
        .map(|(op, id, y)| {
            format!(
                r#"{{"id": "{id}", "op": [{op}], "dst": [{{"x": 0, "y": {y}, "w": 8, "h": 8}}]}}"#
            )
        })
        .collect();
    let d = folder(&graph_json("", &dsts.join(",")));
    let (mut def, h) = read_header(d.path(), "s.json").unwrap();
    let config = Config {
        bga,
        ..Config::defaults(&h)
    };
    let i = skin_beatoraja::import(&mut def, &h, &config, ScreenKind::Result)
        .unwrap_or_else(|e| panic!("{e:?}"));
    let mut r = Res::new(i.skin);
    r.view(&ready(10, true, best(skin_model::ResultNumber::Shown(3))));
    let ours = result_images(&[true, true]);
    let mut drawn: Vec<[u8; 2]> = r
        .drawn(0)
        .into_iter()
        .map(|(t, _)| t)
        .filter(|t| ours.contains(t))
        .collect();
    drawn.sort_unstable();
    drawn
}

/// The images of [`SCREEN_OPS`] whose option holds with the BGA `shown`.
fn result_want(shown: bool) -> Vec<[u8; 2]> {
    result_images(&[!shown, shown])
}

/// The images under 40 and −41 when `which[0]`, those under −40 and 41
/// when `which[1]`, sorted.
fn result_images(which: &[bool; 2]) -> Vec<[u8; 2]> {
    use common::selection::{B0, BAR, LABEL, LAMP};
    let mut v = Vec::new();
    if which[0] {
        v.extend([LAMP, B0]);
    }
    if which[1] {
        v.extend([LABEL, BAR]);
    }
    v.sort_unstable();
    v
}

/// No note names the setting: a play skin whose parts read 40 and 41 by
/// number, sign and name, in `op` and `draw`, imports under every setting
/// with no note but the one any condition known false gives its part,
/// `Ignored` at `op`, one for each part whose option does not hold.
#[test]
fn no_note_names_the_setting() {
    let json = play_json(
        "\"destination\": [\n\
         {\"id\": \"i\", \"op\": [41], \"dst\": [{\"w\": 8, \"h\": 8}]},\n\
         {\"id\": \"i\", \"op\": [-40, \"bgaon\"], \"dst\": [{\"w\": 9, \"h\": 8}]},\n\
         {\"id\": \"i\", \"draw\": \"!bgaoff\", \"dst\": [{\"w\": 10, \"h\": 8}]},\n\
         {\"id\": \"i\", \"draw\": 40, \"dst\": [{\"w\": 11, \"h\": 8}]}]",
    );
    for (bga, shown) in SETTINGS {
        let (_d, i) = imported_in(&json, bga);
        let removed = if shown { 1 } else { 3 };
        assert_eq!(
            common::kinds(&i.report),
            vec![NoteKind::Ignored { field: "op" }; removed],
            "{bga:?}"
        );
    }
}

/// A Lua play skin with source `a` and image `i`, `prelude` after `ms`,
/// and `dsts` as its destinations.
fn lua_play_text(prelude: &str, dsts: &str) -> String {
    format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         {prelude}\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 8, h = 8 }} }}\n\
         skin.destination = {{\n\
         {dsts}\n\
         }}\n\
         return skin\n"
    )
}

/// `text` imported as a Lua play skin under `bga`, with its folder.
fn lua_play(text: &str, bga: Bga) -> (tempfile::TempDir, skin_beatoraja::LuaImported) {
    let d = common::skin(&[("s.luaskin", text)]);
    common::png(d.path(), "a.png", 64, 64);
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let config = Config {
        bga,
        ..Config::default()
    };
    let i = common::lua_play(&h, &config, &cancel).unwrap_or_else(|e| panic!("{e:?}"));
    (d, i)
}

/// A Lua play skin reads the setting in its configured pass, while it
/// builds its tables, and in a `draw` callback the import lowers (the id a
/// numeral, a helper's parameter or a folded sum), with no note: with the
/// BGA shown the parts reading 41 draw and those reading 40 do not,
/// hidden the reverse.
#[test]
fn a_lua_play_skin_reads_the_setting() {
    let dsts = "{ id = \"i\", dst = { { w = w, h = 10 } } },\n\
        { id = \"i\", draw = function() return ms.option(41) end, dst = { { w = 20, h = 10 } } },\n\
        { id = \"i\", draw = function() return on(40) end, dst = { { w = 21, h = 10 } } },\n\
        { id = \"i\", draw = function() return ms.option(39 + 2) end, dst = { { w = 22, h = 10 } } },\n\
        { id = \"i\", draw = function() return not ms.option(41) end, dst = { { w = 23, h = 10 } } },";
    let text = lua_play_text(
        "local function on(k) return ms.option(k) end\n\
         local w = 10\n\
         if ms.option(40) then w = w + 1 end\n\
         if ms.option(41) then w = w + 100 end\n\
         if ms.option(-41) then w = w + 2 end",
        dsts,
    );
    for (bga, shown) in SETTINGS {
        let at = format!("{:?} {}", bga.mode, bga.autoplay);
        let (_d, i) = lua_play(&text, bga);
        assert!(
            i.callbacks
                .iter()
                .all(|d| *d == skin_beatoraja::lua::Disposition::Lowered),
            "{at}: {:?}",
            i.callbacks
        );
        assert!(
            i.imported.report.notes.is_empty(),
            "{at}: {:?}",
            common::kinds(&i.imported.report)
        );
        let want: &[i32] = if shown { &[20, 22, 110] } else { &[13, 21, 23] };
        assert_eq!(widths(i.imported.skin, Some(true)), want, "{at}");
    }
}
