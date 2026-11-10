//! Lua callbacks through the public entries (design.md "beatoraja Lua
//! Skins", per-frame closures): the four callback shapes the surveyed
//! skins use, each written here from scratch, the gauge's look bridge
//! through the real import and play, and the refusals that need no
//! private witness. Every fixture is a small self-made skin.

mod common;

use common::play::Drawn;
use rules::GaugeValue;
use skin_beatoraja::lua::{Disposition, Unsupported};
use skin_beatoraja::{
    Cancel, Config, ImportError, ImportLimit, LuaImported, LuaWhy, NoteKind, read_lua_header,
};
use skin_model::{ChartText, GaugeLook, Layout, MAX_TEXT_BYTES};
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

/// A skin with one text in a font file, `ref` 10 beside its `value`, drawn
/// with `timer`.
fn text_folder(value: &str, timer: &str) -> TempDir {
    let d = folder(&skin(&format!(
        "skin.font = {{ {{ id = \"f\", path = \"f.ttf\" }} }}\n\
         skin.text = {{ {{ id = \"t\", font = \"f\", size = 10, ref = 10, value = {value} }} }}\n\
         table.insert(skin.destination, {{ id = \"t\", {timer} dst = {{ {{ w = 64, h = 10 }} }} }})"
    )));
    common::font(d.path(), "f.ttf");
    d
}

/// The lines a text of `value` asks for over `chart`, once imported.
fn text_lines(value: &str, chart: &ChartText<'_>) -> Vec<String> {
    let i = import(&text_folder(value, ""), 905).unwrap();
    assert_eq!(counts(&i), [1, 0, 0, 0], "{value}");
    i.imported
        .skin
        .text_requests(chart, [1_920, 1_080], &Layout::AUTHOR)
        .into_iter()
        .map(|r| r.text)
        .collect()
}

const FULL: ChartText<'static> = ChartText {
    title: "T",
    subtitle: "S",
    artist: "A",
    subartist: "B",
    genre: "G",
};
const NO_TEXT: ChartText<'static> = ChartText {
    title: "",
    subtitle: "",
    artist: "",
    subartist: "",
    genre: "",
};

/// `main_state.text(n)` as a whole value is that field, not its `ref`.
#[test]
fn a_direct_text_value_is_its_field() {
    for (id, want) in [
        (10, "T"),
        (11, "S"),
        (12, "T S"),
        (13, "G"),
        (14, "A"),
        (15, "B"),
        (16, "A B"),
    ] {
        let value = format!("function() return ms.text({id}) end");
        assert_eq!(text_lines(&value, &FULL), [want], "{id}");
        assert_eq!(text_lines(&value, &NO_TEXT), [] as [&str; 0], "{id}");
    }
}

/// Both written forms of a field's string for when it is empty, a full
/// field's emptiness after its join, a string captured or literal.
#[test]
fn a_text_fallback_shows_for_an_empty_field() {
    for value in [
        "function() local tx = ms.text(12)\n\
           if not tx or tx == \"\" then return \"none\" else return tx end end",
        "function() local tx = ms.text(12)\n\
           if tx ~= \"\" then return tx else return \"none\" end end",
    ] {
        assert_eq!(text_lines(value, &FULL), ["T S"], "{value}");
        assert_eq!(text_lines(value, &NO_TEXT), ["none"], "{value}");
        let subtitle = ChartText {
            subtitle: "S",
            ..NO_TEXT
        };
        assert_eq!(text_lines(value, &subtitle), [" S"], "{value}");
    }
    // A string captured from the chunk, cleaned as a literal is.
    let d = folder(&skin(
        "local none = \"no\\7ne\"\n\
         skin.font = { { id = \"f\", path = \"f.ttf\" } }\n\
         skin.text = { { id = \"t\", font = \"f\", size = 10, value = function()\n\
           local tx = ms.text(13)\n\
           if tx == \"\" then return none else return tx end end } }\n\
         table.insert(skin.destination, { id = \"t\", dst = { { w = 64, h = 10 } } })",
    ));
    common::font(d.path(), "f.ttf");
    let i = import(&d, 905).unwrap();
    assert_eq!(counts(&i), [1, 0, 0, 0]);
    let lines = i
        .imported
        .skin
        .text_requests(&NO_TEXT, [1_920, 1_080], &Layout::AUTHOR);
    assert_eq!(lines[0].text, "none");
    // An empty string for the empty field is the field alone.
    let value = "function() local tx = ms.text(10)\n\
       if tx == \"\" then return \"\" else return tx end end";
    assert_eq!(text_lines(value, &FULL), ["T"]);
    assert_eq!(text_lines(value, &NO_TEXT), [] as [&str; 0]);
}

/// The scene's constants fold in a text's value and record no read: a
/// table line written as the source's table skins write it folds to its
/// last string, its concatenation and course arms never evaluated.
#[test]
fn a_table_line_folds_to_its_literal() {
    let value = "function()\n\
       if not ms.option(290) then\n\
         local tx1, tx2 = ms.text(1001), ms.text(1002)\n\
         if not tx1 or tx1 == \"\" and not tx2 or tx2 == \"\" then\n\
           local tx3 = ms.text(1003)\n\
           if not tx3 or tx3 == \"\" then return \"no table\" else return tx3 end\n\
         else return tx1 .. \" > \" .. tx2 end\n\
       elseif ms.option(289) then return \"last\"\n\
       else return \"course\" end end";
    assert_eq!(text_lines(value, &NO_TEXT), ["no table"]);
    let negated = "function() if ms.option(-1008) then return \"x\" else return \"y\" end end";
    assert_eq!(text_lines(negated, &NO_TEXT), ["x"]);
}

/// A field's string for when it is empty is charged as the field in a
/// bitmap font: a quad per byte of its cap.
#[test]
fn a_text_fallback_is_charged_as_its_field() {
    let d = folder(&skin(
        "skin.font = { { id = \"f\", path = \"f.fnt\" } }\n\
         skin.text = { { id = \"t\", font = \"f\", size = 16, value = function()\n\
           local tx = ms.text(10)\n\
           if tx == \"\" then return \"a\" else return tx end end } }\n\
         table.insert(skin.destination, { id = \"t\", dst = { { w = 64, h = 16 } } })",
    ));
    common::write(
        d.path(),
        "f.fnt",
        b"info face=\"t\" size=16 padding=0,0,0,0\n\
          common lineHeight=16 base=16 scaleW=64 scaleH=64 pages=1\n\
          page id=0 file=\"a.png\"\nchars count=1\n\
          char id=97 x=0 y=0 width=16 height=16 xoffset=0 yoffset=0 xadvance=16 page=0 chnl=15\n",
    );
    let i = import(&d, 905).unwrap();
    assert_eq!(counts(&i), [1, 0, 0, 0]);
    assert_eq!(
        i.imported.report.usage.widget_quads,
        u32::try_from(MAX_TEXT_BYTES).unwrap()
    );
}

/// An empty result draws nothing, as an empty constant.
#[test]
fn an_empty_text_value_is_ignored() {
    for value in [
        "function() return \"\" end",
        "function() return ms.text(1003) end",
    ] {
        let i = import(&text_folder(value, ""), 905).unwrap();
        assert_eq!(counts(&i), [1, 0, 0, 0], "{value}");
        assert_eq!(i.imported.report.usage.texts, 0, "{value}");
        assert!(
            i.imported
                .report
                .notes
                .iter()
                .any(|n| n.kind == NoteKind::Ignored { field: "value" }),
            "{value}"
        );
    }
}

/// `value`'s expected disposition `why`: a decorative text is replaced
/// with that reason in its note, never its `ref`, and failure presentation
/// refuses with it.
#[track_caller]
fn text_follows_its_role(value: &str, why: Unsupported) {
    let i = import(&text_folder(value, ""), 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Replaced(why)], "{value}");
    assert!(
        i.imported.report.notes.iter().any(|n| n.kind
            == NoteKind::Callback {
                field: "value",
                why
            }),
        "{value}"
    );
    assert_eq!(i.imported.report.usage.texts, 0, "no `ref`: {value}");
    match import(&text_folder(value, "timer = 3,"), 905) {
        Err(ImportError::Lua {
            why:
                LuaWhy::Critical {
                    field: "value",
                    why: w,
                },
            ..
        }) => assert_eq!(w, why, "{value}"),
        other => panic!("{value}: {other:?}"),
    }
}

/// A result with no shape a text takes has no consumer.
#[test]
fn other_text_shapes_follow_the_role() {
    let consumer = Unsupported::Consumer("text value");
    for value in [
        "function() return 1 end",
        "function() return nil end",
        "function() return ms.text(1) end",
        "function() local n = ms.number(110) return \"x\" end",
        "function() return ms.number(110) == 0 and \"a\" or \"b\" end",
        "function() if ms.option(271) then return ms.text(10) else return ms.text(11) end end",
        "function() local tx = ms.text(10)\n\
           if tx == \"\" then return tx else return \"x\" end end",
        "function() local a, b = ms.text(10), ms.text(11)\n\
           if a == \"\" then return \"x\" else return b end end",
        "function() local tx = ms.text(12) return tx ~= \"\" and tx or \"none\" end",
    ] {
        text_follows_its_role(value, consumer);
    }
}

/// A value whose reads show a critical part is critical, whatever else it
/// is: it refuses, by the reason it did not fold for, never its `ref`.
#[test]
fn a_text_value_reading_a_critical_part_refuses() {
    let consumer = Unsupported::Consumer("text value");
    for (value, why) in [
        (
            "function() return ms.number(101) == 0 and \"a\" or \"b\" end",
            consumer,
        ),
        (
            "function() local n = ms.number(101) return \"a\" end",
            consumer,
        ),
        (
            "function() return ms.number(101) .. \"a\" end",
            Unsupported::Construct(".."),
        ),
    ] {
        match import(&text_folder(value, ""), 905) {
            Err(ImportError::Lua {
                why:
                    LuaWhy::Critical {
                        field: "value",
                        why: w,
                    },
                ..
            }) => assert_eq!(w, why, "{value}"),
            other => panic!("{value}: {other:?}"),
        }
    }
}

/// Under failure presentation a value that folds is drawn, as anywhere.
#[test]
fn a_failure_presentation_text_folds() {
    let value = "function() local tx = ms.text(10)\n\
       if tx == \"\" then return \"none\" else return tx end end";
    let i = import(&text_folder(value, "timer = 3,"), 905).unwrap();
    assert_eq!(counts(&i), [1, 0, 0, 0]);
    let lines = i
        .imported
        .skin
        .text_requests(&NO_TEXT, [1_920, 1_080], &Layout::AUTHOR);
    assert_eq!(lines[0].text, "none");
}

/// An evaluation that stopped gives its own reason.
#[test]
fn a_stopped_text_value_keeps_its_reason() {
    for (value, why) in [
        (
            "function() return ms.text(12) .. \"x\" end",
            Unsupported::Construct(".."),
        ),
        (
            "function() return string.upper(ms.text(12)) end",
            Unsupported::Construct("library call"),
        ),
        (
            "function() local tx = ms.text(12)\n\
               if tx == \"x\" then return \"y\" else return tx end end",
            Unsupported::Type,
        ),
    ] {
        text_follows_its_role(value, why);
    }
}

/// Reading a text in another consumer keeps its result: the comparison
/// and the scene's constants are the text's alone.
#[test]
fn other_consumers_keep_their_text_reads() {
    for draw in [
        "function() return ms.text(12) == \"\" end",
        "function() return ms.text(1003) == \"\" end",
    ] {
        let d = folder(&skin(&image_at(0, &format!("draw = {draw},"))));
        let i = import(&d, 905).unwrap();
        assert_eq!(
            i.callbacks,
            [Disposition::Replaced(Unsupported::Type)],
            "{draw}"
        );
    }
    let d = folder(&skin(
        "skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8,\n\
           value = function() if ms.text(12) == \"\" then return 1 else return 0 end end } }\n\
         table.insert(skin.destination, { id = \"v\", dst = { { x = 0, y = 100, w = 6, h = 6 } } })",
    ));
    let i = import(&d, 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Replaced(Unsupported::Type)]);
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
    // In range at the end, out of range on the way: past `i32`, which the
    // expressions hold, but within Â±2^53, where the program's 64-bit
    // integers and the source's doubles agree, so the program takes it.
    let d = folder(&skin(&format!(
        "{}\n{}",
        "skin.gauge = { id = \"g\", nodes = { \"n0\", \"n1\", \"n2\", \"n3\" }, parts = 10 }",
        "table.insert(skin.destination, { id = \"g\", draw = function()\n\
           local big = ms.number(110) * 1000\n\
           return big * 1000 - big * 1000 + 1 > 0\n\
         end, dst = { { x = 100, y = 500, w = 100, h = 10 } } })"
    )));
    assert_eq!(import(&d, 905).unwrap().callbacks, [Disposition::Compiled]);
    // Past Â±2^53, critical: refused.
    let d = folder(&skin(&format!(
        "{}\n{}",
        "skin.gauge = { id = \"g\", nodes = { \"n0\", \"n1\", \"n2\", \"n3\" }, parts = 10 }",
        "table.insert(skin.destination, { id = \"g\", draw = function()\n\
           local big = ms.number(110) * 1000000\n\
           return big * big * big - big * big * big + 1 > 0\n\
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

/// A number `v` whose value callback is `value`, drawn at y 100.
fn value_skin(value: &str) -> String {
    skin(&format!(
        "skin.value = {{ {{ id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8,\n\
           value = {value} }} }}\n\
         table.insert(skin.destination, {{ id = \"v\", dst = {{ {{ x = 0, y = 100, w = 6, h = 6 }} }} }})"
    ))
}

/// The remaining-note ratios: the remaining notes over the total, a float
/// division, times 100 and 10 000. Both compile into the program, and the
/// numbers show the source's conversion of the float as the counts move.
#[test]
fn remaining_note_ratios_compile_into_the_program() {
    let remain = "local function remain()\n\
           return ms.number(106) - ms.number(110) - ms.number(111) - ms.number(112)\n\
             - ms.number(113) - ms.number(114)\n\
         end\n";
    let d = folder(&skin(&format!(
        "{remain}\
         skin.value = {{\n\
           {{ id = \"r\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 3,\n\
             value = function() return remain() / ms.number(106) * 100 end }},\n\
           {{ id = \"q\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 5,\n\
             value = function() return remain() / ms.number(106) * 10000 end }} }}\n\
         table.insert(skin.destination, {{ id = \"r\", dst = {{ {{ x = 0, y = 100, w = 6, h = 6 }} }} }})\n\
         table.insert(skin.destination, {{ id = \"q\", dst = {{ {{ x = 0, y = 200, w = 6, h = 6 }} }} }})"
    )));
    let i = import(&d, 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled, Disposition::Compiled]);
    // The digits each number draws, by its row.
    let rows = |q: &[skin_model::Quad]| {
        // The source's y is up: rows 100 and 200 of a 1080-high canvas.
        let n = |y: f32| q.iter().filter(|q| (q.rect[1] - y).abs() < 1.0).count();
        (n(974.0), n(874.0))
    };
    let out = |d: &Drawn| {
        let f = d.play.state().fields;
        let at = |k: usize| f[skin_model::field::COUNT + k];
        [at(0), at(1), at(2), at(3)]
    };
    let mut p = Drawn::new(i.imported.skin.clone(), 8);
    let q = p.frame(1_000, &[]);
    // Nothing judged: 8 / 8 * 100.
    assert_eq!(out(&p), [100, 1, 10_000, 1]);
    assert_eq!(rows(&q), (3, 5));
    // Three of eight judged: 62.5 and 6 250, the float truncated.
    p.tally.counts.perfect = 2;
    p.tally.counts.bad = 1;
    let q = p.frame(1_016, &[]);
    assert_eq!(out(&p), [62, 1, 6_250, 1]);
    assert_eq!(rows(&q), (2, 4));
    // No notes: 0 / 0 is NaN, which the source's conversion makes 0.
    let mut p = Drawn::new(i.imported.skin, 0);
    let q = p.frame(1_000, &[]);
    assert_eq!(out(&p), [0, 1, 0, 1]);
    assert_eq!(rows(&q), (1, 1));
}

/// What the program cannot prove is refused by role, also where the
/// result discards it (Lua runs it all the same): an integer that could
/// leave ±2^53, and a number's value converting a `math.floor` of a
/// float, whose integer has no bound; each control compiles. The source's
/// `%` by 0 is NaN, which the program keeps (no `//`: it is not in the
/// source's grammar and opens the candidate).
#[test]
fn the_program_refuses_what_it_cannot_prove() {
    for (value, want) in [
        (
            "function() return 10 % ms.number(110) end",
            Disposition::Compiled,
        ),
        (
            "function() local n = ms.number(110) return n * n * n * n * 1.0 end",
            Disposition::Replaced(Unsupported::Range),
        ),
        (
            "function() local n = ms.number(110) local unused = n * n * n * n return 1.0 end",
            Disposition::Replaced(Unsupported::Range),
        ),
        (
            "function() local n = ms.number(110) return n * n * 1.0 end",
            Disposition::Compiled,
        ),
        // Discarded, the result an integer or a folded truthiness: the
        // expression backend would take it, but the product is reached.
        (
            "function() local n = ms.number(110) local unused = n * n * n * n return 1 end",
            Disposition::Replaced(Unsupported::Range),
        ),
        (
            "function() local n = ms.number(110) local u = n * n * n * n return u ~= nil end",
            Disposition::Replaced(Unsupported::Range),
        ),
        // The source's `%` rounds once its dividend nears 2^53: its
        // remainder is then of no known bound, so the product is refused.
        (
            "function() return ((ms.number(110) - 9007199254740991) % 5) * 2251799813685247 end",
            Disposition::Replaced(Unsupported::Range),
        ),
        (
            "function() return math.floor(ms.number(110) / 3) end",
            Disposition::Replaced(Unsupported::Range),
        ),
    ] {
        let i = import(&folder(&value_skin(value)), 905).unwrap();
        assert_eq!(i.callbacks, [want], "{value}");
    }
    // A draw only compares the floor, which needs no bound.
    let draw = "draw = function() return math.floor(ms.number(110) / 3) > 1 end,";
    let i = import(&folder(&skin(&image_at(0, draw))), 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled]);
}

/// The numbers are the source's doubles: `(n + 0.5) % 0.1` is exactly 0
/// there for every count, where Lua 5.4's `fmod` gives about 0.1, so the
/// value shows 0 and the draw is false.
#[test]
fn a_float_modulo_shows_what_the_source_shows() {
    let d = folder(&skin(&format!(
        "{}\n{}",
        "skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8,\n\
           value = function() return (ms.number(110) + 0.5) % 0.1 * 1000 end } }\n\
         table.insert(skin.destination, { id = \"v\", dst = { { x = 0, y = 100, w = 6, h = 6 } } })",
        image_at(
            0,
            "draw = function() return (ms.number(110) + 0.5) % 0.1 > 0 end,"
        ),
    )));
    let i = import(&d, 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled, Disposition::Compiled]);
    let mut p = Drawn::new(i.imported.skin, 8);
    for n in 0..6 {
        p.tally.counts.perfect = n;
        p.frame(1_000 + i64::from(n) * 16, &[]);
        let f = p.play.state().fields;
        let at = |k: usize| f[skin_model::field::COUNT + k];
        assert_eq!([at(0), at(1), at(2), at(3)], [0, 1, 0, 1], "{n}");
    }
}

/// The import charges each program callback's per-frame cost as the
/// skin counts it (V5): its instructions, its reads at each use and its
/// gate. Nothing is dropped, so the operations charged are the ones kept.
#[test]
fn a_program_callback_is_charged_as_the_skin_counts_it() {
    let d = folder(&skin(
        "skin.value = {\n\
           { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8,\n\
             value = function() return 5 end },\n\
           { id = \"w\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8,\n\
             value = function() return (ms.number(110) + ms.number(110)) % 3 end } }\n\
         table.insert(skin.destination, { id = \"v\", draw = function() return ms.number(110) / 2 >= 0 end,\n\
           dst = { { x = 0, y = 100, w = 6, h = 6 } } })\n\
         table.insert(skin.destination, { id = \"w\", dst = { { x = 0, y = 200, w = 6, h = 6 } } })",
    ));
    let i = import(&d, 905).unwrap();
    let mut got = i.callbacks.clone();
    got.sort_by_key(|d| format!("{d:?}"));
    // The draw is gated by the expression value's sentinel.
    assert_eq!(
        got,
        [
            Disposition::Compiled,
            Disposition::Compiled,
            Disposition::Lowered
        ]
    );
    assert_eq!(i.imported.skin.program_callbacks(), 2);
    assert_eq!(
        i.imported.report.usage.ops as usize,
        i.imported.skin.ops().total()
    );
}

/// The program's per-frame cost counts against the skin's operation cap
/// at import: as many program draws as fit import, every operation kept
/// charged, and one more is refused there as the operation limit.
#[test]
fn a_program_past_the_operation_cap_is_refused_at_import() {
    let body = |n: usize| {
        skin(&format!(
            "for k = 1, {n} do\n\
               table.insert(skin.destination, {{ id = \"i\", draw = function()\n\
                 return ms.number(110) % 7 + ms.number(111) % 5 > 3\n\
               end, dst = {{ {{ x = 0, y = 0, w = 64, h = 64 }} }} }})\n\
             end"
        ))
    };
    let ops = |n: usize| {
        let i = import(&folder(&body(n)), 905).unwrap();
        assert_eq!(i.callbacks, vec![Disposition::Compiled; n]);
        let charged = i.imported.report.usage.ops as usize;
        assert_eq!(charged, i.imported.skin.ops().total(), "{n}");
        charged
    };
    let (one, two) = (ops(1), ops(2));
    let per = two - one;
    let fit = 1 + (skin_model::MAX_SKIN_OPS - one) / per;
    assert_eq!(ops(fit), one + (fit - 1) * per);
    let e = import(&folder(&body(fit + 1)), 905).unwrap_err();
    assert!(
        matches!(
            e,
            ImportError::Limit {
                limit: ImportLimit::Ops,
                ..
            }
        ),
        "{e:?}"
    );
}

/// A read whose value is discarded still keeps its availability: with
/// the play level unavailable the number draws nothing, as the source's
/// `number(96)` would have no value.
#[test]
fn a_discarded_read_keeps_its_availability() {
    let d = folder(&value_skin(
        "function() local unused = ms.number(96) return 1.0 end",
    ));
    let i = import(&d, 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled]);
    let chart = common::play::chart(8);
    for (level, drawn) in [(None, false), (Some(12), true)] {
        let mut p = Drawn::over_level(i.imported.skin.clone(), &chart, level);
        let q = p.frame(1_000, &[]);
        assert_eq!(!q.is_empty(), drawn, "{level:?}");
    }
}

/// A number whose expression value is `i32::MAX` draws nothing and skips
/// the program draw after it (`SkinNumber.java:135-146`); any other value
/// runs it.
#[test]
fn an_expression_sentinel_skips_a_program_draw() {
    for (value, drawn) in [("2147483647", false), ("5", true)] {
        let d = folder(&skin(&format!(
            "skin.value = {{ {{ id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8,\n\
               value = function() return {value} end }} }}\n\
             table.insert(skin.destination, {{ id = \"v\", draw = function() return ms.number(110) / 2 >= 0 end,\n\
               dst = {{ {{ x = 0, y = 100, w = 6, h = 6 }} }} }})"
        )));
        let i = import(&d, 905).unwrap();
        // Dispositions are by callable; the value lowers, the draw compiles.
        let mut got = i.callbacks.clone();
        got.sort_by_key(|d| format!("{d:?}"));
        assert_eq!(
            got,
            [Disposition::Compiled, Disposition::Lowered],
            "{value}"
        );
        let mut p = Drawn::new(i.imported.skin, 8);
        let q = p.frame(1_000, &[]);
        let ok = p.play.state().fields[skin_model::field::COUNT + 1];
        assert_eq!((!q.is_empty(), ok == 1), (drawn, drawn), "{value}");
    }
}

/// The program has no gating of one draw callback by another: a second
/// program draw on one element is refused by role.
#[test]
fn a_second_program_draw_follows_its_role() {
    let i = import(
        &folder(&skin(&image_at(
            0,
            "op = { function() return ms.number(110) / 2 >= 0 end },\n\
             draw = function() return ms.number(111) / 2 >= 0 end,",
        ))),
        905,
    )
    .unwrap();
    // Dispositions are by callable: the first consumed compiles.
    let mut got = i.callbacks.clone();
    got.sort_by_key(|d| format!("{d:?}"));
    assert_eq!(
        got,
        [
            Disposition::Compiled,
            Disposition::Replaced(Unsupported::Construct("a draw after a program draw"))
        ]
    ); // The image is dropped after its `op` callback was compiled and
    // charged: the callback stays in the program, and every operation
    // the skin keeps was charged.
    assert!(
        i.imported
            .report
            .notes
            .iter()
            .any(|n| n.severity == skin_beatoraja::Severity::Dropped)
    );
    assert_eq!(i.imported.skin.program_callbacks(), 1);
    let kept = i.imported.skin.ops().total();
    assert!(kept > 0);
    assert_eq!(i.imported.report.usage.ops as usize, kept);
}

/// A latch: a closure whose frame-time body writes its own upvalue
/// `state` (a located write), drawn by a decorative image.
const LATCH: &str = "local latch = (function()\n\
      local state = ms.timer_off_value\n\
      return function()\n\
        if state == ms.timer_off_value and ms.number(100) == 200000 then\n\
          state = ms.time()\n\
        end\n\
        return state\n\
      end\n\
    end)()\n\
    table.insert(skin.destination, { id = \"i\", draw = function() return latch() ~= 0 end,\n\
      dst = { { x = 0, y = 0, w = 64, h = 64 } } })";

/// How many callables lowered, compiled or were replaced for a write or
/// an interference.
fn tally(i: &LuaImported) -> (usize, usize, usize) {
    let n = |f: fn(&Disposition) -> bool| i.callbacks.iter().filter(|d| f(d)).count();
    (
        n(|d| matches!(d, Disposition::Lowered | Disposition::Compiled)),
        n(|d| *d == Disposition::Replaced(Unsupported::Writes)),
        n(|d| *d == Disposition::Replaced(Unsupported::Interference)),
    )
}

/// A frame-time write to an upvalue voids only the proofs through that
/// cell: the critical gauge pulse beside a latch writing its own `state`
/// still reads the host module through its upvalue and lowers, and shows
/// as it does alone; the latch's root writes and is replaced.
#[test]
fn a_write_to_another_cell_leaves_a_proof_whole() {
    let d = folder(&skin(&format!("{LATCH}\n{SHAPE_C}")));
    let i = import(&d, 905).unwrap();
    assert_eq!(tally(&i), (1, 1, 0));
    let mut play = Drawn::with_look(i.imported.skin, 10, GaugeLook::Hazard);
    let pulse = play
        .frame(1_000, &[])
        .iter()
        .filter(|q| q.rect[0] >= 499.0)
        .count();
    assert!(pulse > 0);
}

/// A write that may reach a proof's path voids it: through a table entry
/// (not located, so the candidate opens at the write) and to the cell the
/// reader's binding lives in (located): the rebound `ms` is of an unknown
/// value wherever a root reads it, so its index in the pulse opens the
/// candidate there, and the critical pulse refuses at that read.
#[test]
fn a_write_on_a_proofs_path_voids_it() {
    // The host module held in a table, its entry written at frame time.
    let entry = "local holder = { m = ms }\n\
        table.insert(skin.destination, { id = \"i\", draw = function() holder.m = nil return true end,\n\
          dst = { { x = 0, y = 0, w = 64, h = 64 } } })";
    // The reader's own upvalue `ms`, rebound at frame time.
    let cell = "table.insert(skin.destination, { id = \"i\", draw = function() ms = nil return true end,\n\
          dst = { { x = 0, y = 0, w = 64, h = 64 } } })";
    for (writer, line) in [(entry, 12), (cell, 16)] {
        let d = folder(&skin(&format!("{writer}\n{SHAPE_C}")));
        match import(&d, 905) {
            Err(ImportError::Lua {
                at: Some(at),
                why:
                    LuaWhy::Critical {
                        why: Unsupported::Interference,
                        ..
                    },
            }) => assert_eq!(at.line, line, "{writer}"),
            other => panic!("{writer}: {other:?}"),
        }
    }
}

/// State one callback writes and another reads (a value storing the id it
/// shows in a chunk local, a draw reading it): the writer and the reader
/// follow their role, decorative constants here, and an unrelated
/// critical callback lowers.
#[test]
fn a_callback_reading_a_written_cell_follows_its_role() {
    let d = folder(&skin(&format!(
        "local shown\n\
         skin.value = {{ {{ id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4,\n\
           value = function() shown = 152 return 0 end }} }}\n\
         table.insert(skin.destination, {{ id = \"v\", dst = {{ {{ x = 0, y = 100, w = 6, h = 6 }} }} }})\n\
         table.insert(skin.destination, {{ id = \"i\", draw = function() return shown ~= nil end,\n\
           dst = {{ {{ x = 0, y = 0, w = 64, h = 64 }} }} }})\n\
         {SHAPE_C}"
    )));
    let i = import(&d, 905).unwrap();
    assert_eq!(tally(&i), (1, 1, 1));
}

/// An open reason that is not a located write keeps the whole candidate
/// open: beside the latch, a method call opens it, and the critical pulse
/// refuses at the method call.
#[test]
fn an_unlocated_opener_still_opens_the_candidate() {
    let opener = "table.insert(skin.destination, { id = \"i\", draw = function() return (\"x\"):len() > 0 end,\n\
          dst = { { x = 0, y = 0, w = 64, h = 64 } } })";
    let d = folder(&skin(&format!("{LATCH}\n{opener}\n{SHAPE_C}")));
    match import(&d, 905) {
        Err(ImportError::Lua {
            at: Some(at),
            why:
                LuaWhy::Critical {
                    why: Unsupported::Interference,
                    ..
                },
        }) => assert_eq!(at.line, 22),
        other => panic!("{other:?}"),
    }
}

/// An id read from a cell frame time writes resolves to its captured value
/// and the integer numerals written to it (the Rm shape: a value choosing
/// 152 or 153 and showing it): decorative ids leave the writer a decorative
/// constant; a critical one among them refuses; a cell written anything
/// else resolves nothing, which refuses as an unknown id does (the
/// writer's reason, `Writes`, named).
#[test]
fn an_id_through_a_written_cell_resolves_to_what_is_written() {
    let shape = |assign: &str| {
        folder(&skin(&format!(
            "local shown\n\
             skin.value = {{ {{ id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4,\n\
               value = function() if ms.option(1) then {assign} else shown = 153 end return ms.number(shown) end }} }}\n\
             table.insert(skin.destination, {{ id = \"v\", dst = {{ {{ x = 0, y = 100, w = 6, h = 6 }} }} }})\n\
             {SHAPE_C}"
        )))
    };
    let i = import(&shape("shown = 152"), 905).unwrap();
    assert_eq!(tally(&i), (1, 1, 0));
    for (assign, why) in [
        ("shown = 101", Unsupported::Writes),
        ("shown = ms.number(1)", Unsupported::Writes),
    ] {
        match import(&shape(assign), 905) {
            Err(ImportError::Lua {
                why: LuaWhy::Critical { why: w, .. },
                ..
            }) => assert_eq!(w, why, "{assign}"),
            other => panic!("{assign}: {other:?}"),
        }
    }
}

/// The source's `%` of small integers is exact and below its divisor, so a
/// product of it compiles and shows the source's value.
#[test]
fn a_small_modulo_composes() {
    let d = folder(&value_skin(
        "function() return (ms.number(110) % 5) * 1000 end",
    ));
    let i = import(&d, 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled]);
    let mut p = Drawn::new(i.imported.skin, 8);
    for n in 0..7 {
        p.tally.counts.perfect = n;
        p.frame(1_000 + i64::from(n) * 16, &[]);
        let f = p.play.state().fields;
        let at = |k: usize| f[skin_model::field::COUNT + k];
        let want = i32::try_from(n % 5 * 1000).unwrap();
        assert_eq!([at(0), at(1)], [want, 1], "{n}");
    }
}

/// A written cell is never read as its captured value, wherever the
/// analysis reads one. The effect walk: a decorative root rebinding a
/// helper to one that writes the configuration, then calling it: the call
/// is of an unknown function, which opens the candidate, so the critical
/// configuration reader refuses. The role scan's join: a setter rebound to
/// one writing a critical id is unknown too.
#[test]
fn a_rebound_callee_is_never_its_captured_value() {
    // A helper `f`, its replacement `poison` writing the configuration,
    // the decorative `callers`, and the critical configuration reader.
    let rebound = |callers: &str| {
        format!(
            "local cfg = skin_config\n\
             local f = function() return true end\n\
             local poison = function() cfg.option.fc = 905 return true end\n\
             {callers}\n\
             skin.gauge = {{ id = \"g\", nodes = {{ \"n0\", \"n1\", \"n2\", \"n3\" }}, parts = 10 }}\n\
             table.insert(skin.destination, {{ id = \"g\", draw = function() return en() end,\n\
               dst = {{ {{ x = 100, y = 500, w = 100, h = 10 }} }} }})"
        )
    };
    let poison = rebound(
        "table.insert(skin.destination, { id = \"i\", draw = function() f = poison return f() end,\n\
           dst = { { x = 0, y = 0, w = 64, h = 64 } } })",
    );
    match import(&folder(&skin(&poison)), 906) {
        Err(ImportError::Lua {
            why:
                LuaWhy::Critical {
                    why: Unsupported::Interference,
                    ..
                },
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
    // The rebinding met after the call: the walk repeats until no new cell
    // is written, so the earlier root's call is of an unknown function too.
    let later = rebound(
        "table.insert(skin.destination, { id = \"i\", draw = function() return f() end,\n\
           dst = { { x = 0, y = 0, w = 64, h = 64 } } })\n\
         table.insert(skin.destination, { id = \"i\", draw = function() f = poison return true end,\n\
           dst = { { x = 0, y = 0, w = 64, h = 64 } } })",
    );
    assert!(
        matches!(
            import(&folder(&skin(&later)), 906),
            Err(ImportError::Lua {
                why: LuaWhy::Critical {
                    why: Unsupported::Interference,
                    ..
                },
                ..
            })
        ),
        "the call before its rebinding"
    );
    // The integer join: `id` gets 153 directly and 101 through a setter
    // the root rebinds; the critical 101 is never missed.
    let setter = "local id = 152\n\
        local f = function() return true end\n\
        local setid = function() id = 101 return true end\n\
        skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4,\n\
          value = function() id = 153 f = setid f() return ms.number(id) end } }\n\
        table.insert(skin.destination, { id = \"v\", dst = { { x = 0, y = 100, w = 6, h = 6 } } })";
    assert!(import(&folder(&skin(setter)), 905).is_err());
}

/// A critical callback's refusal is located at the write that voided its
/// own proof, whatever the role scan reads afterwards: with `x` written on
/// line 12 and `y` on line 14, a reader of `x` that reads `y` only in a
/// branch never taken refuses at line 12.
#[test]
fn an_interference_is_located_at_the_proofs_write() {
    let body = "local x, y = 1, 1\n\
        table.insert(skin.destination, { id = \"i\", draw = function() x = 2 return true end,\n\
          dst = { { x = 0, y = 0, w = 64, h = 64 } } })\n\
        table.insert(skin.destination, { id = \"i\", draw = function() y = 3 return true end,\n\
          dst = { { x = 0, y = 0, w = 64, h = 64 } } })\n\
        skin.gauge = { id = \"g\", nodes = { \"n0\", \"n1\", \"n2\", \"n3\" }, parts = 10 }\n\
        table.insert(skin.destination, { id = \"g\", draw = function()\n\
            if false then return ms.number(y) > 0 end\n\
            return x == 1\n\
          end, dst = { { x = 100, y = 500, w = 100, h = 10 } } })";
    match import(&folder(&skin(body)), 905) {
        Err(ImportError::Lua {
            at: Some(at),
            why:
                LuaWhy::Critical {
                    why: Unsupported::Interference,
                    ..
                },
        }) => assert_eq!(at.line, 12),
        other => panic!("{other:?}"),
    }
}

/// The integer sources a play skin's callback can reach have a range, and
/// arithmetic past ±2^53 over it is refused, discarded or not; a source
/// with no mapped range (a timer's start or off value) is of no known
/// bound, and the gauge's type is a Java `int`. Rows by source, most with
/// a control that stays within the range (a timer's control compares it
/// with the off value, the only use that bounds it). Not covered here:
/// `event_index`, mapped off play only (the selection and result
/// profiles, `-1` to `10`), and `//`, which no closed candidate holds.
#[test]
fn every_integer_source_is_bounded() {
    let range = Disposition::Replaced(Unsupported::Range);
    for (source, value, want) in [
        // A literal: its own value; one past 2^53 does not fold.
        (
            "literal",
            "function() return ms.number(110) * 4503599627 * 1.0 end",
            range,
        ),
        (
            "literal",
            "function() return ms.number(110) * 2 * 1.0 end",
            Disposition::Compiled,
        ),
        // A captured integer: its value.
        (
            "captured",
            "function() return ms.number(110) * K * 1.0 end",
            range,
        ),
        (
            "captured",
            "function() return (K + 1) * 1.0 end",
            Disposition::Compiled,
        ),
        // A mapped number read: its field's range.
        (
            "number",
            "function() local n = ms.number(110) local u = n * n * n * n return 1 end",
            range,
        ),
        (
            "number",
            "function() local n = ms.number(110) local u = n * n return 1 end",
            Disposition::Lowered,
        ),
        // A judgment count: its count's range.
        (
            "judge",
            "function() local j = ms.judge(1) local u = j * j * j * j return 1 end",
            range,
        ),
        (
            "judge",
            "function() local j = ms.judge(1) local u = j * j return 1 end",
            Disposition::Lowered,
        ),
        // A timer read: of no known bound (its start or the off value).
        (
            "timer",
            "function() local t = ms.timer(0) local unused = t * t return 1 end",
            range,
        ),
        (
            "timer",
            "function() local t = ms.timer(0) local unused = t * t return unused ~= nil end",
            range,
        ),
        (
            "timer",
            "function() local t = ms.timer(0) local u = -t return 1 end",
            range,
        ),
        (
            "timer",
            "function() local t = ms.timer(0) local u = t + 1 return 1 end",
            range,
        ),
        (
            "timer",
            "function() local t = ms.timer(0) local u = t == ms.timer_off_value return 1 end",
            Disposition::Lowered,
        ),
        // The gauge's type: a Java `int`.
        (
            "gauge_type",
            "function() local g = ms.gauge_type() local u = g * g * g return 1 end",
            range,
        ),
        (
            "gauge_type",
            "function() local g = ms.gauge_type() local u = g + 1 return 1 end",
            Disposition::Lowered,
        ),
        // Operators: each result's range from its operands'; a select's
        // the hull of its arms (here converted as a number's value; the
        // effect walk admits no arithmetic on a select of integers, its
        // arms' kinds unknown).
        (
            "math.abs",
            "function() local t = ms.timer(0) local u = math.abs(t) return 1 end",
            range,
        ),
        (
            "math.abs",
            "function() local u = math.abs(ms.number(110)) * 2 return 1 end",
            Disposition::Lowered,
        ),
        (
            "math.max",
            "function() local t = ms.timer(0) local u = math.max(t, 1) * 2 return 1 end",
            range,
        ),
        (
            "select",
            "function() if ms.option(271) then return ms.timer(0) end return 0 end",
            range,
        ),
        (
            "math.floor",
            "function() local u = math.floor(ms.number(110) / 3) * 2 return 1 end",
            range,
        ),
        (
            "%",
            "function() local u = (ms.number(110) % 7) * 1000 return 1 end",
            Disposition::Lowered,
        ),
    ] {
        let body = format!(
            "local K = 4503599627370496\n\
             skin.value = {{ {{ id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8,\n\
               value = {value} }} }}\n\
             table.insert(skin.destination, {{ id = \"v\", dst = {{ {{ x = 0, y = 100, w = 6, h = 6 }} }} }})"
        );
        let i = import(&folder(&skin(&body)), 905)
            .unwrap_or_else(|e| panic!("{source}: {value}: {e:?}"));
        assert_eq!(i.callbacks, [want], "{source}: {value}");
    }
}

/// A timer closure made by a factory whose state is `locals`, running
/// `body`, the timer of `n` images (`i` at x 0, 100, ...).
fn timer_skin(locals: &str, body: &str, n: usize) -> TempDir {
    let dsts: String = (0..n)
        .map(|k| {
            format!(
                "table.insert(skin.destination, {{ id = \"i\", timer = latch,\n\
                   dst = {{ {{ x = {}, y = 0, w = 64, h = 64 }} }} }})\n",
                k * 100
            )
        })
        .collect();
    folder(&skin(&format!(
        "local latch = (function()\n\
           {locals}\n\
           return function()\n\
             {body}\n\
           end\n\
         end)()\n\
         {dsts}"
    )))
}

/// The latch's state.
const STATE: &str = "local state = ms.timer_off_value";

/// The latch's body: the state clock stored once the point reaches
/// 200 000 (a full score), its start the timer.
const LATCH_BODY: &str = "if state == ms.timer_off_value and ms.number(100) == 200000 then state = ms.time() end\n\
     return state";

/// The slots' values.
fn bank(p: &Drawn) -> Vec<skin_model::present::Val> {
    p.play.bank().values().to_vec()
}

/// A full score over `notes` notes: every note a PERFECT, the max combo
/// all of them.
fn full_score(p: &mut Drawn, notes: u32) {
    p.tally.counts.perfect = notes;
    p.tally.max_combo = notes;
    p.tally.combo = notes;
}

/// The latch shape, the MAX line's timer on two destinations: compiled
/// into the program; dark until the point reaches 200 000, then lit from
/// the state clock of that frame and kept, whatever the score after it,
/// through a rearm; fresh at a retry.
#[test]
fn the_latch_starts_once_and_keeps_its_start() {
    use skin_model::present::{OFF, Val};
    let d = timer_skin(STATE, LATCH_BODY, 2);
    let i = import(&d, 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled]);
    let mut p = Drawn::new(i.imported.skin, 4);
    p.state_us = 1_000_000;
    assert!(p.frame(1_000, &[]).is_empty());
    assert_eq!(bank(&p), [OFF]);
    full_score(&mut p, 4);
    p.state_us = 2_000_000;
    assert_eq!(p.frame(1_016, &[]).len(), 2);
    assert_eq!(bank(&p), [Val::Int(2_000_000)]);
    // The score drops (as no play's does): the start stays.
    p.tally = common::play::tally(0, 0);
    p.state_us = 3_500_000;
    assert_eq!(p.frame(1_032, &[]).len(), 2);
    assert_eq!(bank(&p), [Val::Int(2_000_000)]);
    p.play.rearm();
    assert_eq!(bank(&p), [Val::Int(2_000_000)]);
    assert_eq!(p.frame(0, &[]).len(), 2);
    // A retry prepares the play again: dark until reached again.
    p.retry(4);
    assert_eq!(bank(&p), [OFF]);
    assert!(p.frame(1_000, &[]).is_empty());
}

/// Every slot's value is computed before any is stored: a run returning
/// the value its slot had as it began gives off on the run that sets it,
/// so the timer is on from the next frame; and a store on a returning
/// path, or in a callback whose result is the off value or false, is
/// kept.
#[test]
fn stores_are_parallel_and_kept_on_every_path() {
    use skin_model::present::Val;
    let off = "ms.timer_off_value";
    let full = "ms.number(100) == 200000";
    let body = format!(
        "local was = state\n\
         if state == {off} and {full} then state = ms.time() end\n\
         return was"
    );
    let d = timer_skin(STATE, &body, 1);
    let i = import(&d, 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled]);
    let mut p = Drawn::new(i.imported.skin, 4);
    full_score(&mut p, 4);
    p.state_us = 7_000;
    // The run that sets the slot gives what it held, off: not drawn.
    assert!(p.frame(1_000, &[]).is_empty());
    assert_eq!(bank(&p), [Val::Int(7_000)]);
    p.state_us = 9_000;
    assert_eq!(p.frame(1_016, &[]).len(), 1);
    assert_eq!(bank(&p), [Val::Int(7_000)]);

    for (body, drawn) in [
        // A store on the arm that returns.
        (
            format!(
                "if state == {off} and {full} then state = ms.time() return 0 end\nreturn state"
            ),
            true,
        ),
        // A store in a run giving the off value, or false.
        (
            format!("if state == {off} and {full} then state = ms.time() end\nreturn {off}"),
            false,
        ),
        (
            format!("if state == {off} and {full} then state = ms.time() end\nreturn false"),
            true,
        ),
    ] {
        let d = timer_skin(STATE, &body, 1);
        let i = import(&d, 905).unwrap();
        assert_eq!(i.callbacks, [Disposition::Compiled], "{body}");
        let mut p = Drawn::new(i.imported.skin, 4);
        full_score(&mut p, 4);
        p.state_us = 5_000;
        assert_eq!(!p.frame(1_000, &[]).is_empty(), drawn, "{body}");
        assert_eq!(bank(&p), [Val::Int(5_000)], "{body}");
    }
}

/// A slot is taken only for a proven cell, domain and writer; otherwise
/// the timer follows its role (a decorative constant here): a cell two
/// instances of a factory capture, a counter, a write a dropped
/// destination's timer would make, a consumer with a condition, a write
/// that can happen before play (no tally guard), a write repeated once
/// set or of the off value, a cell only a helper captures, one a helper
/// shared by two roots writes (each helper reading a written cell:
/// `Interference`), a guard reading another slot or the clock, a writer
/// that raises, and one a root reads through a helper.
#[test]
fn a_slot_is_taken_only_when_proven() {
    let off = "ms.timer_off_value";
    let full = "ms.number(100) == 200000";
    let construct = |s| Disposition::Replaced(Unsupported::Construct(s));
    let one = |body: &str| {
        let d = folder(&skin(body));
        import(&d, 905).unwrap_or_else(|e| panic!("{body}: {e:?}"))
    };
    let dst = |name: &str, extra: &str| {
        format!(
            "table.insert(skin.destination, {{ id = \"i\", timer = {name}, {extra}\n\
               dst = {{ {{ x = 0, y = 0, w = 64, h = 64 }} }} }})\n"
        )
    };
    let latch = |locals: &str, body: &str| {
        format!("local latch = (function()\n{locals}\nreturn function()\n{body}\nend\nend)()\n")
    };
    // Two instances of one factory.
    let two = format!(
        "local function mk() local s = {off}\n\
           return function() if s == {off} and {full} then s = ms.time() end return s end end\n\
         local l1 = mk()\nlocal l2 = mk()\n{}{}",
        dst("l1", ""),
        dst("l2", "")
    );
    let i = one(&two);
    assert!(
        i.callbacks
            .contains(&construct("a written cell more than one closure captures")),
        "{:?}",
        i.callbacks
    );
    assert!(
        !i.callbacks.contains(&Disposition::Compiled),
        "{:?}",
        i.callbacks
    );
    for (body, why) in [
        // A counter: the walk reads the written cell as unknown, so its
        // arithmetic opens the candidate (and the residual refuses
        // arithmetic on a slot as `Range`).
        (
            latch("local n = 0", "n = n + 1 return 0") + &dst("latch", ""),
            construct("a slot in a candidate that is not closed"),
        ),
        // A destination whose timer it is, dropped before its timer for
        // what only the import refuses (the source draws it): the latch
        // would not run on every frame the source runs it.
        (
            latch(STATE, LATCH_BODY)
                + &dst("latch", "")
                + &dst("latch", "mouseRect = { x = 0, y = 0, w = 1, h = 1 },"),
            construct("a slot writer a dropped destination calls"),
        ),
        // A consumer with a condition.
        (
            latch(STATE, LATCH_BODY)
                + &dst("latch", "draw = function() return ms.number(110) > 2 end,"),
            construct("a slot writer the program does not run on every play frame"),
        ),
        // Written while loading: no tally in its guard.
        (
            latch(
                STATE,
                &format!("if state == {off} then state = ms.time() end return state"),
            ) + &dst("latch", ""),
            construct("a slot write that can happen outside play frames"),
        ),
        // Set again once set: no off test.
        (
            latch(
                STATE,
                &format!("if {full} then state = ms.time() end return state"),
            ) + &dst("latch", ""),
            construct("a slot written again once set"),
        ),
        (
            latch(
                STATE,
                &format!("if state == {off} and {full} then state = {off} end return state"),
            ) + &dst("latch", ""),
            construct("a slot written the off value"),
        ),
        // A cell only a helper captures and writes: the helper reads it,
        // a written cell, first.
        (
            format!(
                "local s = {off}\n\
                 local function h() if s == {off} and {full} then s = ms.time() end return s end\n\
                 local latch = function() h() return h() end\n{}",
                dst("latch", "")
            ),
            Disposition::Replaced(Unsupported::Interference),
        ),
        // A helper two roots share writes it.
        (
            format!(
                "local s = {off}\n\
                 local function h() if s == {off} and {full} then s = ms.time() end return s end\n\
                 local r1 = function() return h() end\n\
                 local r2 = function() return h() end\n{}{}",
                dst("r1", ""),
                dst("r2", "")
            ),
            Disposition::Replaced(Unsupported::Interference),
        ),
        // A guard reading another slot: set by the source while loading
        // (`b` starts set) where the program sets it at the first play
        // frame, or enabled the run after the other slot changes, after
        // the last play frame.
        (
            latch(
                &format!("local a = {off}\nlocal b = 1"),
                &format!(
                    "if a == {off} and b ~= {off} then a = ms.time() end\n\
                     if b == {off} and {full} then b = ms.time() end\n\
                     return a"
                ),
            ) + &dst("latch", ""),
            construct("a slot write guarded by another slot"),
        ),
        (
            latch(
                &format!("local a = {off}\nlocal b = {off}"),
                &format!(
                    "local was = a\n\
                     if a == {off} and {full} then a = ms.time() end\n\
                     if b == {off} and was ~= {off} and {full} then b = ms.time() end\n\
                     return b"
                ),
            ) + &dst("latch", ""),
            construct("a slot write guarded by another slot"),
        ),
        // A guard reading the clock, which runs on outside play (the
        // practice page).
        (
            latch(
                STATE,
                &format!(
                    "if state == {off} and {full} and ms.time() > 10000000 then state = ms.time() end\n\
                     return state"
                ),
            ) + &dst("latch", ""),
            construct("a slot write guarded by state that changes outside play"),
        ),
        // A guard reading the EX score, which the source reads as
        // `Integer.MIN_VALUE` before the first judgment (so this latch is
        // set while loading there): off the guard allowlist.
        (
            latch(
                STATE,
                &format!(
                    "if state == {off} and ms.number(71) < 0 then state = ms.time() end
                     return state"
                ),
            ) + &dst("latch", ""),
            construct("a slot write guarded by a read off its allowlist"),
        ),
        // The latch also a custom timer, which the source runs before
        // every object: a consumer the program does not run.
        (
            latch(STATE, LATCH_BODY)
                + "skin.customTimers = { { id = 3001, timer = latch } }
" + &dst("latch", ""),
            construct("a slot writer referenced beside its destinations' timers"),
        ),
        // A writer with a definite Lua error on a reached path: outside
        // the admission walk's subset, it opens the candidate, which
        // grants no slot (and the residual refuses it as a Lua error), so
        // no slot writer the program keeps raises.
        (
            latch(
                STATE,
                &format!(
                    "local x = nil\n\
                     if state == {off} and {full} then state = ms.time() end\n\
                     return x + 1"
                ),
            ) + &dst("latch", ""),
            construct("a slot in a candidate that is not closed"),
        ),
        // The root reads its cell through a helper.
        (
            format!(
                "local s = {off}\n\
                 local function peek() return s end\n\
                 local latch = function()\n\
                   if s == {off} and {full} then s = ms.time() end return peek() end\n{}",
                dst("latch", "")
            ),
            construct("a written cell more than one closure captures"),
        ),
    ] {
        let i = one(&body);
        assert!(i.callbacks.contains(&why), "{body}: {:?}", i.callbacks);
        assert!(
            !i.callbacks.contains(&Disposition::Compiled),
            "{body}: {:?}",
            i.callbacks
        );
    }
}

/// A slot-owning timer callback is charged as the skin counts it (V5:
/// both runs and the slot it touches): nothing is dropped, so the
/// operations charged are the ones kept.
#[test]
fn a_timer_callback_is_charged_as_the_skin_counts_it() {
    let i = import(&timer_skin(STATE, LATCH_BODY, 1), 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled]);
    assert_eq!(i.imported.skin.program_callbacks(), 1);
    assert_eq!(
        i.imported.report.usage.ops as usize,
        i.imported.skin.ops().total()
    );
}

/// The loading proof reads a slot at its initial value: an integer start
/// fails the latch's own off test before play, so a guard of that test
/// alone is no write while loading, and the timer shows the start.
#[test]
fn an_integer_initialized_slot_is_judged_at_its_initial_value() {
    use skin_model::present::Val;
    let off = "ms.timer_off_value";
    let d = timer_skin(
        "local state = 5000",
        &format!(
            "if state == {off} then state = ms.time() end
return state"
        ),
        1,
    );
    let i = import(&d, 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled]);
    let mut p = Drawn::new(i.imported.skin, 4);
    p.state_us = 9_000;
    assert_eq!(p.frame(1_000, &[]).len(), 1);
    assert_eq!(bank(&p), [Val::Int(5_000)]);
}

/// Slots are granted only in a closed candidate: a closure the analysis
/// never captures (one held under a numeric key, one a metatable's
/// `__index` gives, one published at run time into a table or a global)
/// can write the slot or call the root, so each opens the candidate and
/// the root follows its role.
#[test]
fn a_slot_needs_a_closed_candidate() {
    let root = "local state = 5000000\n\
                local function timer() state = state return state end\n";
    let dst = "table.insert(skin.destination, { id = \"i\", timer = timer,\n\
                 dst = { { x = 0, y = 0, w = 64, h = 64 } } })\n";
    let custom = |call: &str| {
        format!("skin.customTimers = {{ {{ id = 3001, timer = function() {call} end }} }}\n")
    };
    let writer = "function() state = 7000000 return timer() end";
    for (name, escape) in [
        (
            "numeric key",
            format!(
                "local hidden = {{ {writer} }}\n{}",
                custom("return hidden[1]()")
            ),
        ),
        (
            "metatable",
            format!(
                "local holder = setmetatable({{}}, {{ __index = function() state = 7000000 return timer end }})\n{}",
                custom("return holder.f()")
            ),
        ),
        (
            "table publication",
            format!(
                "local pub = {{}}\nlocal function publish() pub.f = {writer} end\n{}",
                custom("publish() return pub.f()")
            ),
        ),
        (
            "global publication",
            format!(
                "local function publish() published = {writer} end\n{}",
                custom("publish() return published()")
            ),
        ),
    ] {
        let d = folder(&skin(&format!("{root}{escape}{dst}")));
        let i = import(&d, 905).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert!(
            i.callbacks
                .contains(&Disposition::Replaced(Unsupported::Construct(
                    "a slot in a candidate that is not closed"
                ))),
            "{name}: {:?}",
            i.callbacks
        );
        assert!(
            !i.callbacks.contains(&Disposition::Compiled),
            "{name}: {:?}",
            i.callbacks
        );
    }
    // The control: the same root alone compiles.
    let d = folder(&skin(&format!("{root}{dst}")));
    assert_eq!(import(&d, 905).unwrap().callbacks, [Disposition::Compiled]);
}

/// Two destinations of one latch returning the value its slot had as the
/// run began: on the setting frame the first is off (it set the slot) and
/// the second on (it saw it set); both on after.
#[test]
fn a_return_old_latch_lights_its_second_destination_first() {
    let off = "ms.timer_off_value";
    let body = format!(
        "local was = state\n\
         if state == {off} and ms.number(100) == 200000 then state = ms.time() end\n\
         return was"
    );
    let d = timer_skin(STATE, &body, 2);
    let i = import(&d, 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled]);
    let mut p = Drawn::new(i.imported.skin, 4);
    full_score(&mut p, 4);
    p.state_us = 7_000;
    let q = p.frame(1_000, &[]);
    // Only the second destination, at x 100 of 1920 (the canvas's left
    // edge is 0): the first is off.
    assert_eq!(q.len(), 1);
    assert!(q[0].rect[0] > 50.0, "{:?}", q[0].rect);
    assert_eq!(p.frame(1_016, &[]).len(), 2);
}

/// A practice round's section, 60 s from the chart's start.
fn round() -> eval::Section {
    eval::Section {
        start: bms_model::TimeUs(0),
        end: bms_model::TimeUs(60_000_000),
        lead_in: bms_model::TimeUs(0),
        initial_gauges: vec![common::play::GAUGES[0].start],
    }
}

/// The program timer's start as of the last build, the last timer slot.
fn program_start(p: &Drawn) -> Option<bms_model::TimeUs> {
    let last = p.play.state().timers.len() - 1;
    p.play.timer(u16::try_from(last).unwrap())
}

/// Codex's integer slot, shown only while the point is positive.
fn positive_point_body() -> String {
    let off = "ms.timer_off_value";
    format!(
        "if state == {off} then state = ms.time() end\n\
         if ms.number(100) > 0 then return state end\n\
         return {off}"
    )
}

/// A fresh section attempt (a play state's first round) runs its slot
/// program from the initial slots, the point 0 before its first judgment:
/// the latch lights at a full score.
#[test]
fn a_fresh_section_attempt_runs_its_slots() {
    use skin_model::present::{OFF, Val};
    let d = timer_skin(STATE, LATCH_BODY, 1);
    let i = import(&d, 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled]);
    let mut p = Drawn::over_section(i.imported.skin.clone(), 4, &round());
    p.state_us = 2_000_000;
    assert!(p.frame(1_000, &[]).is_empty());
    assert_eq!(bank(&p), [OFF]);
    full_score(&mut p, 4);
    assert_eq!(p.frame(1_016, &[]).len(), 1);
    assert_eq!(bank(&p), [Val::Int(2_000_000)]);
}

/// A carried round with valid slots runs its slot program on them from
/// its first frame: the latch round 1 set at 7 s keeps that start (2 s of
/// elapsed at 9 s), where a fresh round is dark until a full score.
#[test]
fn a_carried_round_keeps_its_latch() {
    use skin_model::Continuation;
    use skin_model::present::Val;
    let d = timer_skin(STATE, LATCH_BODY, 1);
    let skin = import(&d, 905).unwrap().imported.skin;
    let mut r1 = Drawn::over_section(skin.clone(), 4, &round());
    full_score(&mut r1, 4);
    r1.state_us = 7_000_000;
    r1.complete = true;
    assert_eq!(r1.frame(1_000, &[]).len(), 1);
    assert_eq!(bank(&r1), [Val::Int(7_000_000)]);
    let carry = r1.play.carry();
    assert!(carry.slots_run && carry.terminal);
    let mut r2 = Drawn::over_carried(
        skin.clone(),
        4,
        &round(),
        Continuation::Carried {
            point: 200_000,
            bank: Some(carry.bank),
        },
    );
    r2.state_us = 9_000_000;
    assert_eq!(r2.frame(1_000, &[]).len(), 1);
    assert_eq!(bank(&r2), [Val::Int(7_000_000)]);
    assert_eq!(program_start(&r2), Some(bms_model::TimeUs(-1_000_000)));
    // The control: a fresh round at the same point in time is dark.
    let mut fresh = Drawn::over_section(skin, 4, &round());
    fresh.state_us = 9_000_000;
    assert!(fresh.frame(1_000, &[]).is_empty());
}

/// Before a carried round's first judgment the point reads the play
/// state's cached point; from its first judgment, a POOR or an empty
/// POOR, its own. A cache of 0 reads 0.
#[test]
fn the_point_is_cached_until_the_first_judgment() {
    use skin_model::Continuation;
    use skin_model::present::Bank;
    use skin_model::present::Val;
    let d = timer_skin("local state = 5000", &positive_point_body(), 1);
    let i = import(&d, 905).unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled]);
    let skin = i.imported.skin;
    let init = Bank::of(vec![Val::Int(5_000)]);
    let carried = |point| Continuation::Carried {
        point,
        bank: Some(&init),
    };
    for first in ["poor", "empty poor"] {
        let mut p = Drawn::over_carried(skin.clone(), 4, &round(), carried(200_000));
        p.state_us = 6_000_000;
        assert_eq!(p.frame(1_000, &[]).len(), 1, "{first}: before");
        match first {
            "poor" => p.tally.counts.poor = 1,
            _ => p.tally.empty_poor = 1,
        }
        assert!(p.frame(1_016, &[]).is_empty(), "{first}: after");
        assert_eq!(bank(&p), [Val::Int(5_000)]);
    }
    let mut zero = Drawn::over_carried(skin, 4, &round(), carried(0));
    zero.state_us = 6_000_000;
    assert!(zero.frame(1_000, &[]).is_empty());
}

/// A slotted callback reading anything but the point, constants, its slots
/// and the clock is not carry-safe: on a carried state its slot program
/// does not run (its slot stays as carried, its timer off), where a fresh
/// attempt runs it.
#[test]
fn a_program_reading_other_fields_does_not_carry() {
    use skin_model::Continuation;
    use skin_model::present::{Bank, OFF, Val};
    let off = "ms.timer_off_value";
    for read in ["ms.number(71) > 0", &format!("ms.timer(48) ~= {off}")] {
        let body = format!(
            "if state == {off} and ms.number(100) == 200000 then state = ms.time() end\n\
             if {read} then return state end\n\
             return {off}"
        );
        let d = timer_skin(STATE, &body, 1);
        let i = import(&d, 905).unwrap();
        assert_eq!(i.callbacks, [Disposition::Compiled], "{read}");
        let skin = i.imported.skin;
        let mut fresh = Drawn::over_section(skin.clone(), 4, &round());
        full_score(&mut fresh, 4);
        fresh.state_us = 3_000_000;
        let _ = fresh.frame(1_000, &[]);
        assert_eq!(bank(&fresh), [Val::Int(3_000_000)], "{read}: fresh");
        let init = Bank::of(vec![OFF]);
        let mut carried = Drawn::over_carried(
            skin,
            4,
            &round(),
            Continuation::Carried {
                point: 200_000,
                bank: Some(&init),
            },
        );
        full_score(&mut carried, 4);
        carried.state_us = 3_000_000;
        assert!(carried.frame(1_000, &[]).is_empty(), "{read}: carried");
        assert_eq!(bank(&carried), [OFF], "{read}: carried");
        assert!(!carried.play.carry().slots_run, "{read}");
    }
}

/// The clock is carry-safe: a return condition on `time()` crossing its
/// threshold between rounds shows the start round 1 stored.
#[test]
fn a_clock_read_carries() {
    use skin_model::Continuation;
    use skin_model::present::Val;
    let off = "ms.timer_off_value";
    let body = format!(
        "if state == {off} and ms.number(100) == 200000 then state = ms.time() end\n\
         if ms.time() > 5000000 then return state end\n\
         return {off}"
    );
    let d = timer_skin(STATE, &body, 1);
    let skin = import(&d, 905).unwrap().imported.skin;
    let mut r1 = Drawn::over_section(skin.clone(), 4, &round());
    full_score(&mut r1, 4);
    r1.state_us = 1_000_000;
    r1.complete = true;
    assert!(r1.frame(1_000, &[]).is_empty());
    assert_eq!(bank(&r1), [Val::Int(1_000_000)]);
    let carry = r1.play.carry();
    assert!(carry.terminal);
    let mut r2 = Drawn::over_carried(
        skin,
        4,
        &round(),
        Continuation::Carried {
            point: 200_000,
            bank: Some(carry.bank),
        },
    );
    r2.state_us = 6_000_000;
    assert_eq!(r2.frame(1_000, &[]).len(), 1);
    assert_eq!(bank(&r2), [Val::Int(1_000_000)]);
    // 5 s of elapsed at 1 s of chart time.
    assert_eq!(program_start(&r2), Some(bms_model::TimeUs(-4_000_000)));
}

/// An unwitnessed carry (no valid slots) runs no slot program: the latch
/// stays dark at a full score, its slot at its initial value.
#[test]
fn an_unwitnessed_round_runs_no_slot_program() {
    use skin_model::Continuation;
    use skin_model::present::OFF;
    let d = timer_skin(STATE, LATCH_BODY, 1);
    let skin = import(&d, 905).unwrap().imported.skin;
    let mut p = Drawn::over_carried(
        skin,
        4,
        &round(),
        Continuation::Carried {
            point: 200_000,
            bank: None,
        },
    );
    full_score(&mut p, 4);
    p.state_us = 2_000_000;
    p.complete = true;
    assert!(p.frame(1_000, &[]).is_empty());
    assert_eq!(bank(&p), [OFF]);
    let carry = p.play.carry();
    assert!(!carry.slots_run && !carry.terminal);
}

/// A rearm keeps the slots and the cached point and clears the first
/// judgment's latch: after a judgment and a rearm the point reads the
/// cache again.
#[test]
fn a_rearm_keeps_the_cache() {
    use skin_model::Continuation;
    use skin_model::present::{Bank, Val};
    let d = timer_skin("local state = 5000", &positive_point_body(), 1);
    let skin = import(&d, 905).unwrap().imported.skin;
    let init = Bank::of(vec![Val::Int(5_000)]);
    let mut p = Drawn::over_carried(
        skin,
        4,
        &round(),
        Continuation::Carried {
            point: 200_000,
            bank: Some(&init),
        },
    );
    p.state_us = 6_000_000;
    assert_eq!(p.frame(1_000, &[]).len(), 1);
    p.tally.counts.poor = 1;
    assert!(p.frame(1_016, &[]).is_empty());
    p.play.rearm();
    p.tally = common::play::tally(0, 0);
    assert_eq!(p.frame(1_032, &[]).len(), 1);
    assert_eq!(bank(&p), [Val::Int(5_000)]);
}

/// A carried bank is checked against the program before use: another
/// length, or a value of the right length outside its slot's domain,
/// refuses the attempt.
#[test]
fn a_continuation_of_another_shape_is_refused() {
    use skin_model::present::{Bank, OFF, Val};
    use skin_model::{Continuation, PrepareError};
    let d = timer_skin(STATE, LATCH_BODY, 1);
    let skin = import(&d, 905).unwrap().imported.skin;
    let chart = common::play::chart(4);
    let section = round();
    let ctx = skin_model::PlayContext {
        attempt: skin_model::Attempt::Section(&section),
        ..common::play::context_with(None, None)
    };
    for vals in [vec![], vec![OFF, OFF], vec![Val::Int(-5)]] {
        let b = Bank::of(vals.clone());
        let carry = Continuation::Carried {
            point: 0,
            bank: Some(&b),
        };
        assert_eq!(
            skin.prepare_carried(&chart, &ctx, carry).err(),
            Some(PrepareError::Continuation),
            "{vals:?}"
        );
    }
    // The control: a value in the domain is taken.
    let b = Bank::of(vec![Val::Int(5)]);
    let carry = Continuation::Carried {
        point: 0,
        bank: Some(&b),
    };
    assert!(skin.prepare_carried(&chart, &ctx, carry).is_ok());
}
