//! The application clock on play: numbers 20 (the frames presented in
//! the last second), 21-26 (the local date and time) and 27-29 (the
//! uptime since launch) read the frame's application readouts, on play
//! and loading frames, by id and by name, JSON and Lua alike.

mod common;

use common::play::{Drawn, Loader, round};
use common::{import_body, write};
use skin_beatoraja::lua::Disposition;
use skin_beatoraja::{Cancel, Config, read_lua_header};
use skin_model::{AppReadouts, LocalTime};

/// Readouts whose every part is told apart from its neighbours: 59 fps,
/// 2026-10-12 21:47:58, and 3 725 999 ms since launch (1 h, 2 min, 5 s).
const APP: AppReadouts = AppReadouts {
    fps: 59,
    uptime_us: 3_725_999_000,
    local: LocalTime {
        year: 2_026,
        month: 10,
        day: 12,
        hour: 21,
        minute: 47,
        second: 58,
    },
};

/// Each id, its name and what it shows under [`APP`].
const SHOWN: [(i32, &str, &str); 10] = [
    (20, "current_fps", "59"),
    (21, "currenttime_year", "2026"),
    (22, "currenttime_month", "10"),
    (23, "currenttime_day", "12"),
    (24, "currenttime_hour", "21"),
    (25, "currenttime_minute", "47"),
    (26, "currenttime_saecond", "58"),
    (27, "boottime_hour", "1"),
    (28, "boottime_minute", "2"),
    (29, "boottime_second", "5"),
];

/// An EX score of one cell at y = 0, then number `prop` of four cells at
/// y = 50.
fn app_number(prop: &str) -> common::Loaded {
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

/// C7-C9 on import: each of numbers 20 to 29, by id and by name, is kept
/// with no note and shows the frame's application readouts on a play frame
/// and on a loading frame (class Live), following them from frame to
/// frame.
#[test]
fn numbers_20_to_29_show_the_application_readouts() {
    for (id, name, want) in SHOWN {
        for prop in [format!(r#""ref": {id}"#), format!(r#""value": "{name}""#)] {
            let l = app_number(&prop);
            assert!(
                l.report().notes.is_empty(),
                "{prop}: {:?}",
                l.report().notes
            );
            let mut d = l.drawn();
            let glyph = d.ex_glyphs(1_000);
            d.app = APP;
            assert_eq!(digits_of(&glyph, &d.frame(1_000, &[])), want, "{prop}");
            // The next frame's readouts, a second and a minute later.
            d.app = AppReadouts {
                uptime_us: APP.uptime_us + 61_000_000,
                ..APP
            };
            let next = digits_of(&glyph, &d.frame(1_016, &[]));
            match id {
                28 => assert_eq!(next, "3", "{prop}"),
                29 => assert_eq!(next, "6", "{prop}"),
                _ => assert_eq!(next, want, "{prop}"),
            }
            let mut loader: Loader = app_number(&prop).loading();
            loader.app = APP;
            assert_eq!(
                digits_of(&glyph, &loader.frame(100)),
                want,
                "loading {prop}"
            );
        }
    }
}

/// A Lua play skin with one image per draw `k`, `10 + k` wide.
fn lua_skin(draws: &[String]) -> (tempfile::TempDir, skin_beatoraja::LuaHeader) {
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
    (d, h)
}

/// A Lua play callback's `number(n)` for 20 to 29 lowers and reads the
/// frame's readouts: each draw compares one id with its value under
/// [`APP`], and a second set with a value one off, which never draws.
#[test]
fn lua_callbacks_read_the_application_readouts() {
    let draws: Vec<String> = SHOWN
        .iter()
        .flat_map(|(id, _, want)| {
            let v: i32 = want.parse().unwrap();
            [
                format!("return ms.number({id}) == {v}"),
                format!("return ms.number({id}) == {}", v + 1),
            ]
        })
        .collect();
    let (_d, h) = lua_skin(&draws);
    let i = common::lua_play(&h, &Config::default(), &Cancel::new()).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered; 20]);
    let mut p = Drawn::new(i.imported.skin, 10);
    p.app = APP;
    let mut widths: Vec<i32> = p.rects(1_000).iter().map(|r| r[2]).collect();
    widths.sort_unstable();
    let want: Vec<i32> = (0..10).map(|k| 10 + 2 * k).collect();
    assert_eq!(widths, want);
}
