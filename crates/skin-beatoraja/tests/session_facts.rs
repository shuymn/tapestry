//! Session facts and scene constants a play skin reads each frame
//! (docs/stage3.md "Ops and conditions"): the attempt's autoplay through
//! ops 32 and 33, while loading too; course mode, 290, a constant false;
//! the lane option, image index 42, always OFF. JSON and Lua alike.

mod common;

use common::play::{Drawn, Loader, round};
use common::{import_body, write};
use skin_beatoraja::lua::Disposition;
use skin_beatoraja::{Cancel, Config, LuaImported, read_lua_header};

/// An image drawn `w` wide under `ops`.
fn drawn_under(ops: &[(i32, i32)]) -> String {
    let dsts: Vec<String> = ops
        .iter()
        .map(|(op, w)| format!(r#"{{"id": "i", "op": [{op}], "dst": [{{"w": {w}, "h": 10}}]}}"#))
        .collect();
    format!(
        r#""source": [{{"id": "a", "path": "a.png"}}],
           "image": [{{"id": "i", "src": "a", "w": 64, "h": 64}}],
           "destination": [{}]"#,
        dsts.join(", ")
    )
}

fn widths(rects: &[[i32; 4]]) -> Vec<i32> {
    let mut w: Vec<i32> = rects.iter().map(|r| r[2]).collect();
    w.sort_unstable();
    w
}

/// 33 holds in an autoplay attempt and 32 in another, each the other's
/// negation; 290 is false and -290 true whatever the attempt.
#[test]
fn ops_read_the_attempts_autoplay_and_fold_course_mode() {
    let body = drawn_under(&[
        (33, 10),
        (32, 11),
        (-33, 12),
        (-32, 13),
        (290, 14),
        (-290, 15),
    ]);
    for (autoplay, want) in [(true, [10, 13, 15]), (false, [11, 12, 15])] {
        let loaded = import_body(&body);
        let skin = loaded.ok().skin.clone();
        let mut d = Drawn::with_autoplay(skin, 10, autoplay);
        assert_eq!(widths(&d.rects(1_000)), want, "autoplay {autoplay}");
    }
}

/// While loading, the frame's autoplay is read: known, never hidden.
#[test]
fn loading_reads_the_frames_autoplay() {
    let body = drawn_under(&[(33, 10), (32, 11)]);
    let mut l: Loader = import_body(&body).loading();
    assert_eq!(widths(&l.rects(0)), [11]);
    l.autoplay = true;
    assert_eq!(widths(&l.rects(100)), [10]);
}

/// Image index 42, the lane option, selects entry 0 (OFF): never entry 1,
/// never dropped.
#[test]
fn the_lane_option_reads_off() {
    let body = r#""source": [{"id": "a", "path": "a.png"}],
           "image": [{"id": "off", "src": "a", "x": 0, "w": 8, "h": 8},
                     {"id": "on", "src": "a", "x": 8, "w": 8, "h": 8}],
           "imageset": [{"id": "s", "ref": 42, "images": ["off", "on"]}],
           "destination": [{"id": "s", "dst": [{"w": 10, "h": 10}]},
                           {"id": "off", "dst": [{"x": 100, "w": 4, "h": 4}]},
                           {"id": "on", "dst": [{"x": 200, "w": 4, "h": 4}]}]"#;
    let mut d = import_body(body).drawn();
    let quads = d.frame(1_000, &[]);
    let set = quads
        .iter()
        .find(|q| round(q.rect)[2] == 10)
        .expect("drawn");
    let off = quads.iter().find(|q| round(q.rect)[0] == 100).unwrap();
    assert_eq!(set.uv, off.uv);
}

/// A Lua callback's `option(33)` and `option(-290)` lower and read the same
/// facts at frame time.
#[test]
fn lua_callbacks_read_the_same_facts() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "s.luaskin",
        b"local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
          if not skin_config then return skin end\n\
          local ms = require(\"main_state\")\n\
          skin.source = { { id = \"a\", path = \"a.png\" } }\n\
          skin.image = { { id = \"i\", src = \"a\", x = 0, y = 0, w = 8, h = 8 } }\n\
          skin.destination = {\n\
            { id = \"i\", draw = function() return ms.option(33) end, dst = { { w = 10, h = 10 } } },\n\
            { id = \"i\", draw = function() return ms.option(-290) end, dst = { { w = 11, h = 10 } } } }\n\
          return skin\n",
    );
    common::png(d.path(), "a.png", 64, 64);
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let i: LuaImported = common::lua_play(&h, &Config::default(), &cancel).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered, Disposition::Lowered]);
    for (autoplay, want) in [(true, vec![10, 11]), (false, vec![11])] {
        let i = common::lua_play(&h, &Config::default(), &cancel).unwrap();
        let mut p = Drawn::with_autoplay(i.imported.skin, 10, autoplay);
        assert_eq!(widths(&p.rects(1_000)), want, "autoplay {autoplay}");
    }
}
