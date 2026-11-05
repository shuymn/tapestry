//! Lua callbacks through the public entries (design.md "beatoraja Lua
//! Skins", per-frame closures): the four callback shapes the surveyed
//! skins use, each written here from scratch, the gauge's look bridge
//! through the real import and play, and the refusals that need no
//! private witness. Every fixture is a small self-made skin.

mod common;

use common::play::Drawn;
use rules::GaugeValue;
use skin_beatoraja::lua::{Disposition, Unsupported};
use skin_beatoraja::{Cancel, Config, ImportError, LuaImported, LuaWhy, NoteKind, read_lua_header};
use skin_model::GaugeLook;
use tempfile::TempDir;

/// A skin folder whose `s.luaskin` is `entry`, with a 64×64 `a.png`.
fn folder(entry: &str) -> TempDir {
    let d = tempfile::tempdir().unwrap();
    common::write(d.path(), "s.luaskin", entry.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    d
}

/// A play skin with an option `fc` (items 905 and 906, 905 the default)
/// whose configured pass runs `body` with `ms` the host module, the
/// helper `en()` (whether `fc` is 906), source `a`, image `i` and gauge
/// node images `n0`..`n3`.
fn skin(body: &str) -> String {
    format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080,\n\
           property = {{ {{ name = \"fc\", item = {{ {{ name = \"off\", op = 905 }}, {{ name = \"on\", op = 906 }} }} }} }} }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         local function en() return skin_config.option[\"fc\"] == 906 end\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }},\n\
           {{ id = \"n0\", src = \"a\", x = 0, y = 0, w = 8, h = 8 }}, {{ id = \"n1\", src = \"a\", x = 8, y = 0, w = 8, h = 8 }},\n\
           {{ id = \"n2\", src = \"a\", x = 16, y = 0, w = 8, h = 8 }}, {{ id = \"n3\", src = \"a\", x = 24, y = 0, w = 8, h = 8 }} }}\n\
         skin.destination = {{}}\n\
         {body}\n\
         return skin\n"
    )
}

/// Imports `s.luaskin` with the option `fc` set to `fc`.
fn import(d: &TempDir, fc: i32) -> Result<LuaImported, ImportError> {
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel)?;
    let config = Config {
        options: vec![Some(fc)],
        ..Config::default()
    };
    common::lua_play(&h, &config, &cancel)
}

/// How many callables ended each way: lowered, replaced, refused,
/// classified only.
fn counts(i: &LuaImported) -> [usize; 4] {
    let n = |f: fn(&Disposition) -> bool| i.callbacks.iter().filter(|d| f(d)).count();
    [
        n(|d| *d == Disposition::Lowered),
        n(|d| matches!(d, Disposition::Replaced(_))),
        n(|d| *d == Disposition::Refused),
        n(|d| *d == Disposition::ClassifiedOnly),
    ]
}

/// An image `i` drawn at `x` with `fields`.
fn image_at(x: i32, fields: &str) -> String {
    format!(
        "table.insert(skin.destination, {{ id = \"i\", {fields} dst = {{ {{ x = {x}, y = 0, w = 64, h = 64 }} }} }})"
    )
}

/// Shape A: a judge-count indicator, on under option 906, whose
/// condition reads three counts after the option helper.
const SHAPE_A: &str = "op = { 906 }, draw = function()\n\
      local total = ms.number(110)\n\
      local early = ms.number(410)\n\
      local late = ms.number(411)\n\
      return en() and (early == late or total == early)\n\
    end,";

#[test]
fn shape_a_folds_its_option_and_converts_when_it_is_on() {
    let d = folder(&skin(&image_at(0, SHAPE_A)));
    // Off by default: the op drops the element before its condition is
    // read, so the callback is classified only.
    let i = import(&d, 905).unwrap();
    assert_eq!(counts(&i), [0, 0, 0, 1]);
    // On: it converts, the option folded true.
    let i = import(&d, 906).unwrap();
    assert_eq!(counts(&i), [1, 0, 0, 0]);
    assert!(
        !i.imported
            .report
            .notes
            .iter()
            .any(|n| matches!(n.kind, NoteKind::Callback { .. }))
    );
}

#[test]
fn shape_b_converts_a_number_s_arithmetic() {
    let d = folder(&skin(
        "skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8,\n\
           value = function()\n\
             local total = ms.number(106)\n\
             local a = ms.number(110) local b = ms.number(111) local c = ms.number(112)\n\
             local e = ms.number(113) local f = ms.number(114)\n\
             local run = a + b + c + e + f\n\
             return total - run\n\
           end } }\n\
         table.insert(skin.destination, { id = \"v\", dst = { { x = 0, y = 100, w = 6, h = 6 } } })",
    ));
    let i = import(&d, 905).unwrap();
    assert_eq!(counts(&i), [1, 0, 0, 0]);
}

/// Shape C: an additive pulse on the gauge shown at the fifth and sixth
/// gauge types.
const SHAPE_C: &str = "skin.gauge = { id = \"g\", nodes = { \"n0\", \"n1\", \"n2\", \"n3\" }, parts = 10 }\n\
    table.insert(skin.destination, { id = \"g\", dst = { { x = 100, y = 500, w = 100, h = 10 } } })\n\
    table.insert(skin.destination, { id = \"g\", blend = 2, draw = function()\n\
      local kind = ms.gauge_type()\n\
      return kind == 4 or kind == 5\n\
    end, dst = { { x = 500, y = 500, w = 100, h = 10 } } })";

#[test]
fn shape_c_shows_the_pulse_at_the_ex_hard_and_hazard_gauges() {
    let d = folder(&skin(SHAPE_C));
    let i = import(&d, 905).unwrap();
    assert_eq!(counts(&i), [1, 0, 0, 0]);
    // The witness: the real import, played at each look and over gauge
    // values. The pulse (drawn from x 500) shows at ex_hard and hazard
    // only; the fill (from x 100) follows the value.
    for look in GaugeLook::ALL {
        let skin = import(&d, 905).unwrap().imported.skin;
        let mut play = Drawn::with_look(skin, 10, look);
        let mut fills = Vec::new();
        for v in [0, 25, 50, 100] {
            play.gauges[0].value = GaugeValue(v * GaugeValue::PERCENT);
            let quads = play.frame(1_000, &[]);
            let pulse = quads.iter().filter(|q| q.rect[0] >= 499.0).count();
            let want = matches!(look, GaugeLook::ExHard | GaugeLook::Hazard);
            assert_eq!(pulse > 0, want, "{look:?} at {v}");
            let fill: Vec<_> = quads
                .iter()
                .filter(|q| q.rect[0] < 499.0)
                .map(|q| format!("{:?}", q))
                .collect();
            fills.push(fill);
        }
        assert!(
            fills.windows(2).all(|w| w[0] != w[1]),
            "{look:?}: the fill changes with the value"
        );
    }
}

#[test]
fn shape_d_s_unmapped_read_is_a_reported_false() {
    let d = folder(&skin(&image_at(
        0,
        "draw = function()\n local broken = ms.number(426)\n return broken > 0\n end,",
    )));
    let i = import(&d, 905).unwrap();
    assert_eq!(counts(&i), [0, 1, 0, 0]);
    let why = Unsupported::Unmapped { api: 1, id: 426 };
    assert_eq!(i.callbacks, [Disposition::Replaced(why)]);
    assert!(common::has(
        &i.imported.report,
        NoteKind::Callback { field: "draw", why }
    ));
    // Nothing is drawn: the element is dropped, never shown with a 0.
    assert!(Drawn::new(i.imported.skin, 0).frame(1_000, &[]).is_empty());
}

#[test]
fn a_critical_callback_that_does_not_convert_refuses() {
    // The gauge's condition reads an unmapped id.
    let d = folder(&skin(
        "skin.gauge = { id = \"g\", nodes = { \"n0\", \"n1\", \"n2\", \"n3\" }, parts = 10 }\n\
         table.insert(skin.destination, { id = \"g\",\n draw = function() return ms.number(426) > 0 end,\n\
           dst = { { x = 100, y = 500, w = 100, h = 10 } } })",
    ));
    match import(&d, 905) {
        Err(ImportError::Lua {
            why:
                LuaWhy::Critical {
                    field: "draw",
                    why: Unsupported::Unmapped { id: 426, .. },
                },
            at: Some(at),
        }) => assert_eq!(at.line, 13),
        other => panic!("{other:?}"),
    }
    // An EX score read makes a number critical: one that does not convert
    // refuses, though the rest of it reads decorative counts.
    let d = folder(&skin(
        "skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8,\n\
           value = function() return ms.number(101) + ms.number(426) end } }\n\
         table.insert(skin.destination, { id = \"v\", dst = { { x = 0, y = 100, w = 6, h = 6 } } })",
    ));
    assert!(matches!(
        import(&d, 905),
        Err(ImportError::Lua {
            why: LuaWhy::Critical { field: "value", .. },
            ..
        })
    ));
    // The same unsupported read in a decorative number is replaced.
    let d = folder(&skin(
        "skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8,\n\
           value = function() return ms.number(110) + ms.number(426) end } }\n\
         table.insert(skin.destination, { id = \"v\", dst = { { x = 0, y = 100, w = 6, h = 6 } } })",
    ));
    let i = import(&d, 905).unwrap();
    assert_eq!(counts(&i), [0, 1, 0, 0]);
}

/// A play skin whose gauge flash, drawn from x 500, is a `draw` in a part
/// the entry loads with `dofile`, `main_state` a local of the part's
/// chunk: shown at the fifth and sixth gauge types while the option
/// `flash` is 930 (931 the other item).
fn gauge_flash() -> TempDir {
    let d = folder(
        "local skin = { type = 0, name = \"t\", w = 1920, h = 1080,\n\
           property = { { name = \"flash\", item = { { name = \"on\", op = 930 }, { name = \"off\", op = 931 } } } } }\n\
         if not skin_config then return skin end\n\
         skin.source = { { id = \"a\", path = \"a.png\" } }\n\
         skin.image = {\n\
           { id = \"n0\", src = \"a\", x = 0, y = 0, w = 8, h = 8 }, { id = \"n1\", src = \"a\", x = 8, y = 0, w = 8, h = 8 },\n\
           { id = \"n2\", src = \"a\", x = 16, y = 0, w = 8, h = 8 }, { id = \"n3\", src = \"a\", x = 24, y = 0, w = 8, h = 8 } }\n\
         skin.gauge = { id = \"g\", nodes = { \"n0\", \"n1\", \"n2\", \"n3\" }, parts = 10 }\n\
         skin.destination = { { id = \"g\", dst = { { x = 100, y = 500, w = 100, h = 10 } } } }\n\
         for _, d in ipairs(dofile(\"frame/flash.lua\")) do table.insert(skin.destination, d) end\n\
         return skin\n",
    );
    common::write(
        d.path(),
        "frame/flash.lua",
        b"local main_state = require(\"main_state\")\n\
          return { { id = \"g\", blend = 2, draw = function()\n\
            return main_state.gauge_type() == 4 and skin_config.option[\"flash\"] == 930\n\
              or main_state.gauge_type() == 5 and skin_config.option[\"flash\"] == 930\n\
          end, dst = { { x = 500, y = 500, w = 100, h = 10 } } } }\n",
    );
    d
}

#[test]
fn a_critical_callback_folds_configuration() {
    let d = gauge_flash();
    for flash in [930, 931] {
        // The critical gauge `draw` is consumed and lowered, the option
        // folded.
        let i = import(&d, flash).unwrap();
        assert_eq!(i.callbacks, [Disposition::Lowered], "{flash}");
        // Over the six gauge kinds, the flash shows at the fifth and
        // sixth only under 930, and never under 931.
        for look in GaugeLook::ALL {
            let skin = import(&d, flash).unwrap().imported.skin;
            let quads = Drawn::with_look(skin, 10, look).frame(1_000, &[]);
            let flashes = quads.iter().any(|q| q.rect[0] >= 499.0);
            let want = flash == 930 && matches!(look, GaugeLook::ExHard | GaugeLook::Hazard);
            assert_eq!(flashes, want, "{flash} at {look:?}");
        }
    }
}

#[test]
fn intermediates_out_of_range_do_not_convert() {
    // In range at the end, out of range on the way: Lua's 64-bit
    // integers and the expressions' 32-bit ones would agree only by luck.
    let d = folder(&skin(&format!(
        "{}\n{}",
        "skin.gauge = { id = \"g\", nodes = { \"n0\", \"n1\", \"n2\", \"n3\" }, parts = 10 }",
        "table.insert(skin.destination, { id = \"g\", draw = function()\n\
           local big = ms.number(110) * 1000\n\
           return big * 1000 - big * 1000 + 1 > 0\n\
         end, dst = { { x = 100, y = 500, w = 100, h = 10 } } })"
    )));
    assert!(matches!(
        import(&d, 905),
        Err(ImportError::Lua {
            why: LuaWhy::Critical {
                why: Unsupported::Range,
                ..
            },
            ..
        })
    ));
}

#[test]
fn timers_compare_with_the_off_value() {
    let d = folder(&skin(&format!(
        "{}\n{}\n{}",
        image_at(
            0,
            "draw = function() return ms.timer(143) ~= ms.timer_off_value end,"
        ),
        image_at(
            100,
            "draw = function() return ms.timer(41) == ms.timer_off_value end,"
        ),
        image_at(
            200,
            "draw = function() return ms.timer(0) == ms.timer_off_value end,"
        ),
    )));
    let i = import(&d, 905).unwrap();
    assert_eq!(counts(&i), [3, 0, 0, 0]);
    // A timer's value as a number is not converted: the decorative
    // image is replaced.
    let d = folder(&skin(&image_at(
        0,
        "draw = function() return ms.timer(41) > 0 end,",
    )));
    let i = import(&d, 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Replaced(Unsupported::Type)]);
}

/// A draw callback reading the chart's BPM-change condition lowers, with
/// one D4 note, and follows the play context's fact.
#[test]
fn a_bpm_change_callback_reads_the_charts_fact() {
    let d = folder(&skin(&image_at(
        0,
        "draw = function()\n return ms.option(177)\n end,",
    )));
    let i = import(&d, 905).unwrap();
    assert_eq!(counts(&i), [1, 0, 0, 0]);
    let d4 = NoteKind::Deviation(skin_beatoraja::DeviationId::D4);
    assert_eq!(common::kinds(&i.imported.report), [d4]);
    let chart = bms_model::ChartBuilder::new(150_000).build().unwrap();
    for (fact, shown) in [(Some(true), true), (Some(false), false), (None, false)] {
        let skin = i.imported.skin.clone();
        let mut play = Drawn::over_with(skin, &chart, None, fact);
        assert_eq!(play.frame(0, &[]).len(), usize::from(shown), "{fact:?}");
    }
}

#[test]
fn a_configured_offset_folds_into_a_draw() {
    // The offset's entries are plain tables the pass builds, read along a
    // static path from an upvalue: the draw folds to the configured
    // alpha's sign.
    let d = folder(
        "local skin = { type = 0, name = \"t\", w = 1920, h = 1080,\n\
           offset = { { name = \"bga\", id = 40, a = true } } }\n\
         if not skin_config then return skin end\n\
         local offset = { bga = skin_config.offset[\"bga\"] }\n\
         skin.source = { { id = \"a\", path = \"a.png\" } }\n\
         skin.image = { { id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 } }\n\
         skin.destination = { { id = \"i\", draw = function() return offset.bga.a >= 0 end,\n\
           dst = { { x = 0, y = 0, w = 64, h = 64 } } } }\n\
         return skin\n",
    );
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    for (a, shown) in [(0, true), (-1, false)] {
        let mut config = Config::defaults(h.header());
        config.offsets.insert(
            40,
            skin_beatoraja::OffsetValue {
                a,
                ..Default::default()
            },
        );
        let i = common::lua_play(&h, &config, &cancel).unwrap();
        assert_eq!(i.callbacks, [Disposition::Lowered], "{a}");
        let quads = Drawn::new(i.imported.skin, 0).frame(1_000, &[]);
        assert_eq!(!quads.is_empty(), shown, "{a}");
    }
}
