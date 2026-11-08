//! The source's 7-key point a play skin reads each frame through number
//! 100 (`point`), by id and name, and a Lua callback's `number(100)`
//! (design.md "Rendering and Skins", Point).

mod common;

use common::play::Drawn;
use common::{import_body, write};
use skin_beatoraja::lua::Disposition;
use skin_beatoraja::{Cancel, Config, LuaImported, read_lua_header};

/// The tally of `perfect` PERFECTs with the maximum combo `max`.
fn perfects(perfect: u32, max: u32) -> eval::Tally {
    let mut t = common::play::tally(0, 0);
    t.counts.perfect = perfect;
    t.max_combo = max;
    t
}

/// Number 100 shows the point over the attempt's targets, by id and by
/// name, with no note.
#[test]
fn number_100_shows_the_point() {
    for prop in [r#""ref": 100"#, r#""value": "point""#] {
        let loaded = import_body(&format!(
            r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 6,
                          {prop}}}],
               "destination": [{{"id": "v", "dst": [{{"w": 12, "h": 20}}]}}]"#
        ));
        assert!(
            loaded.report().notes.is_empty(),
            "{prop}: {:?}",
            loaded.report().notes
        );
        let mut d = Drawn::new(loaded.ok().skin.clone(), 4);
        // 0 before any judgment.
        assert_eq!(d.rects(0).len(), 1, "{prop}");
        // 4 PERFECTs, the maximum combo 4: 200 000.
        d.tally = perfects(4, 4);
        assert_eq!(d.rects(0).len(), 6, "{prop}");
        // 1 PERFECT, the maximum combo 1: 37 500 + 12 500.
        d.tally = perfects(1, 1);
        assert_eq!(d.rects(0).len(), 5, "{prop}");
    }
}

/// A Lua callback's `number(100) == 200000` lowers and holds once every
/// target is PERFECT with the full maximum combo.
#[test]
fn a_lua_callback_reads_the_point() {
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
            { id = \"i\", draw = function() return ms.number(100) == 200000 end, dst = { { w = 10, h = 10 } } } }\n\
          return skin\n",
    );
    common::png(d.path(), "a.png", 64, 64);
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let i: LuaImported = common::lua_play(&h, &Config::default(), &cancel).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered]);
    let mut drawn = Drawn::new(i.imported.skin, 4);
    drawn.tally = perfects(4, 3);
    assert!(drawn.rects(0).is_empty());
    drawn.tally = perfects(4, 4);
    assert_eq!(drawn.rects(0).len(), 1);
}
