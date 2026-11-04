//! Roots, inert callables and the open analysis (design.md "beatoraja
//! Lua Skins", per-frame closures): which functions and strings of the
//! returned table the source reads as code, by its schema, and what a
//! root the analysis cannot see does to the proofs that rest on captured
//! state. Each test imports through the candidate's two passes.

use super::callback_tests::{gauge, image, import, skin};
use super::{Disposition, LuaImported, LuaWhy, Unsupported};
use crate::error::ImportError;
use crate::import::NoteKind;

/// A frame-time writer: classified, it refuses at its assignment.
const WRITER: &str = "function() n = n + 1 return true end";

/// A critical gauge `draw` reading option `x`, which the body sets to 1.
fn config_gauge() -> String {
    gauge("function() return skin_config.option[\"x\"] == 1 end")
}

/// A skin whose configured pass sets option `x` to 1, declares the
/// writer's counter `n`, then runs `body`, with the critical
/// configuration reader first among the destinations and `dst` after it.
fn beside(body: &str, dst: &str) -> String {
    skin(
        &format!("skin_config.option.x = 1\nlocal n = 0\n{body}"),
        &format!("{}{dst}", config_gauge()),
    )
}

/// Checks that `r` refuses the critical reader in `field` because the
/// analysis is open, located at the open site: a converted value of the
/// entry, line 0.
#[track_caller]
fn interferes(r: Result<LuaImported, ImportError>, field: &str) {
    match r {
        Err(ImportError::Lua {
            why:
                LuaWhy::Critical {
                    field: f,
                    why: Unsupported::Interference,
                },
            at: Some(at),
        }) => {
            assert_eq!(f, field);
            assert_eq!((at.file.as_str(), at.line), ("s.luaskin", 0));
        }
        other => panic!("{other:?}"),
    }
}

/// Checks that `r` imported with the critical reader lowered.
#[track_caller]
fn lowers(r: Result<LuaImported, ImportError>) -> LuaImported {
    let i = r.unwrap_or_else(|e| panic!("{e:?}"));
    assert!(
        i.callbacks.contains(&Disposition::Lowered),
        "{:?}",
        i.callbacks
    );
    i
}

#[test]
fn a_function_the_source_never_reads_is_inert() {
    // An option item's `isSelected` is no field of the source's item: a
    // writer there is converted and never classified, and the critical
    // reader beside it folds.
    let entry = beside(
        &format!(
            "skin.property = {{ {{ name = \"p\", item = {{ {{ name = \"a\", op = 900, isSelected = {WRITER} }} }} }} }}"
        ),
        "",
    );
    let i = lowers(import(&entry));
    assert_eq!(i.callbacks, [Disposition::Lowered, Disposition::Inert]);
    // A function where the source reads a destination record, not a
    // property, is inert too.
    let entry = beside(
        &format!("local f = {WRITER}\nskin.songlist = {{ liston = {{ f }} }}"),
        "",
    );
    let i = lowers(import(&entry));
    assert_eq!(i.callbacks, [Disposition::Lowered, Disposition::Inert]);
    // Met at an ignored key and at a `draw`, in either order, it is a
    // root: classified, and refused at its assignment.
    for key in ["aaa", "zzz"] {
        let entry = beside(
            &format!("local w = {WRITER}\nskin.{key} = {{ w }}"),
            &image("w"),
        );
        assert_eq!(
            super::tests::why(import(&entry)),
            LuaWhy::Effect(Unsupported::Construct("assignment")),
            "{key}"
        );
    }
    // A helper met first where the source never reads it is tagged when
    // a root captures it: the root lowers through it.
    let entry = skin(
        "local function h() return ms.gauge_type() == 4 end\nskin.aaa = { h }",
        &gauge("function() return h() end"),
    );
    let i = import(&entry).unwrap();
    assert_eq!(i.callbacks, [Disposition::Captured, Disposition::Lowered]);
}

#[test]
fn every_op_entry_and_every_array_entry_is_read() {
    // An `op` entry under a string key is a root, as is a `draw` in a
    // destination under one.
    for dst in [
        format!("{{ id = \"i\", op = {{ hidden = {WRITER} }}, dst = {{ {{ w = 6, h = 6 }} }} }},"),
        format!("extra = {{ id = \"i\", draw = {WRITER}, dst = {{ {{ w = 6, h = 6 }} }} }},"),
    ] {
        let entry = beside("", &dst);
        assert_eq!(
            super::tests::why(import(&entry)),
            LuaWhy::Effect(Unsupported::Construct("assignment")),
            "{dst}"
        );
    }
}

#[test]
fn a_root_the_analysis_cannot_see_voids_the_capture_proof() {
    for (body, dst) in [
        // Scripts: a `draw` and a `timer` the source compiles.
        (
            "",
            image("\"(function() skin_config.option.x = 2 return true end)()\""),
        ),
        (
            "",
            "{ id = \"i\", timer = \"(function() skin_config.option.x = 2 return 0 end)()\", \
             dst = { { w = 6, h = 6 } } },"
                .to_owned(),
        ),
        // A string property's and an integer property's names that their
        // own factories do not resolve.
        (
            "skin.text = { { id = \"t\", value = \"score\" } }",
            String::new(),
        ),
        (
            "skin.value = { { id = \"v\", value = \"ranking_exscore+1\" } }",
            String::new(),
        ),
        // A mixed `op` list whose positional entry holds a function.
        (
            "",
            format!(
                "{{ id = \"i\", op = {{ {WRITER}, k = 1 }}, dst = {{ {{ w = 6, h = 6 }} }} }},"
            ),
        ),
    ] {
        interferes(import(&beside(body, &dst)), "draw");
    }
    // The table met first at an ignored key and then at an `op`, or the
    // other way round: its unread entry is seen at the `op` either way.
    for key in ["aaa", "zzz"] {
        let body = format!(
            "local conditions = {{ {WRITER}, keep = function() return true end }}\n\
             skin.{key} = conditions"
        );
        let dst = "{ id = \"i\", op = conditions, dst = { { w = 6, h = 6 } } },";
        interferes(import(&beside(&body, dst)), "draw");
    }
    // A script beside a critical module read: the module's binding is
    // not proven either.
    let entry = skin(
        "",
        &format!(
            "{}{}",
            gauge("function() return ms.gauge_type() == 4 end"),
            image("\"(function() return true end)()\"")
        ),
    );
    interferes(import(&entry), "draw");
    // Controls: a name the field's own factory resolves; a mixed list
    // whose positional entry is a number; a mixed record, whose other
    // entries the source never reads. Each lowers.
    for body in [
        "skin.text = { { id = \"t\", value = \"title\" } }",
        "skin.customEvents = { 1, k = { id = 1000 } }",
        "skin.songlist = { function() end, id = \"s\" }",
    ] {
        lowers(import(&beside(body, "")));
    }
}

#[test]
fn a_font_fallback_with_a_metatable_opens_the_analysis() {
    // The source reads a fallback table's `path` and `type` with its
    // metatable consulted.
    let font = |fallback: &str| {
        format!("skin.font = {{ {{ id = \"f\", path = \"a.ttf\", fallback = {fallback} }} }}")
    };
    let meta = format!("setmetatable({{}}, {{ __index = {WRITER} }})");
    interferes(import(&beside(&font(&format!("{{ {meta} }}")), "")), "draw");
    lowers(import(&beside(&font("{ { path = \"a\" } }"), "")));
    // A mixed fallback list whose positional entry is that table: unread,
    // whether met there directly or first through an ignored key.
    let mixed = format!("local hidden = {meta}\nlocal fb = {{ hidden, keep = \"a.ttf\" }}\n");
    interferes(
        import(&beside(&format!("{mixed}{}", font("fb")), "")),
        "draw",
    );
    for key in ["aaa", "zzz"] {
        let body = format!("{mixed}skin.{key} = fb\n{}", font("fb"));
        interferes(import(&beside(&body, "")), "draw");
    }
}

/// Whether `i` was imported with a note of kind `kind`.
fn noted(i: &LuaImported, kind: NoteKind) -> bool {
    i.imported.report.notes.iter().any(|n| n.kind == kind)
}

/// Checks that `r` refuses the critical callable in `field` for `why`.
#[track_caller]
fn critical(r: Result<LuaImported, ImportError>, field: &str, why: Unsupported) {
    assert_eq!(
        super::tests::why(r),
        LuaWhy::Critical {
            field: match field {
                "op" => "op",
                "timer" => "timer",
                _ => "value",
            },
            why
        }
    );
}

/// [`skin`] with `image` added to its images.
fn with_image(image: &str, dst: &str) -> String {
    skin(&format!("local more = {image}"), dst).replace(
        "skin.gauge = ",
        "skin.image[#skin.image + 1] = more\nskin.gauge = ",
    )
}

/// The read of an unmapped value property.
const UNMAPPED: &str = "function() return ms.number(425) > 0 end";

#[test]
fn an_op_function_is_a_condition_by_its_element_s_role() {
    let op = |id: &str, f: &str| {
        format!("{{ id = \"{id}\", op = {{ {f} }}, dst = {{ {{ w = 64, h = 10 }} }} }},")
    };
    let i = import(&skin("", &op("i", "function() return true end"))).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered]);
    let why = Unsupported::Unmapped { api: 1, id: 425 };
    let i = import(&skin("", &op("i", UNMAPPED))).unwrap();
    assert_eq!(i.callbacks, [Disposition::Replaced(why)]);
    assert!(noted(&i, NoteKind::Callback { field: "op", why }));
    critical(import(&skin("", &op("g", UNMAPPED))), "op", why);
}

#[test]
fn a_timer_function_has_no_consumer() {
    let consumer = Unsupported::Consumer("timer");
    let timed = |id: &str| {
        format!(
            "{{ id = \"{id}\", timer = function() return 0 end, dst = {{ {{ w = 64, h = 10 }} }} }},"
        )
    };
    // A destination's: dropped when decorative, refused when critical.
    let i = import(&skin("", &timed("i"))).unwrap();
    assert_eq!(i.callbacks, [Disposition::Replaced(consumer)]);
    assert!(noted(
        &i,
        NoteKind::Callback {
            field: "timer",
            why: consumer
        }
    ));
    critical(import(&skin("", &timed("g"))), "timer", consumer);
    // A source image's cycle timer, by its element's role: failure
    // presentation (timer 3) is critical.
    let cycled = |timer: &str| {
        with_image(
            "{ id = \"c\", src = \"a\", x = 0, y = 0, w = 64, h = 64, divx = 2, cycle = 100,\n\
             timer = function() return 0 end }",
            &format!("{{ id = \"c\", {timer} dst = {{ {{ x = 0, y = 0, w = 32, h = 64 }} }} }},"),
        )
    };
    let i = import(&cycled("")).unwrap();
    assert_eq!(i.callbacks, [Disposition::Replaced(consumer)]);
    assert!(noted(
        &i,
        NoteKind::Callback {
            field: "timer",
            why: consumer
        }
    ));
    critical(import(&cycled("timer = 3,")), "timer", consumer);
    // A custom timer's: no target, never consumed.
    let i = import(&skin(
        "skin.customTimers = { { id = 1000, timer = function() return 0 end } }",
        "{ id = \"i\", timer = 1000, dst = { { w = 64, h = 64 } } },",
    ))
    .unwrap();
    assert_eq!(i.callbacks, [Disposition::ClassifiedOnly]);
}

#[test]
fn a_string_or_choice_value_function_has_no_consumer_and_no_fallback() {
    // A text's value: never its `ref`.
    let text = "skin.font = { { id = \"f\", path = \"a.ttf\" } }\n\
                skin.text = { { id = \"t\", font = \"f\", size = 10, ref = 10, value = function() return 1 end } }";
    let consumer = Unsupported::Consumer("text value");
    let shown =
        |timer: &str| format!("{{ id = \"t\", {timer} dst = {{ {{ w = 64, h = 10 }} }} }},");
    let i = import(&skin(text, &shown(""))).unwrap();
    assert_eq!(i.callbacks, [Disposition::Replaced(consumer)]);
    assert!(noted(
        &i,
        NoteKind::Callback {
            field: "value",
            why: consumer
        }
    ));
    critical(import(&skin(text, &shown("timer = 3,"))), "value", consumer);
    // An image set's value: by its role, which its reads also give.
    let set = |f: &str| {
        format!(
            "skin.imageset = {{ {{ id = \"s\", ref = 10, images = {{ \"i\", \"i\" }}, value = {f} }} }}"
        )
    };
    let consumer = Unsupported::Consumer("image set value");
    let shown = "{ id = \"s\", dst = { { w = 64, h = 64 } } },";
    let i = import(&skin(&set("function() return 1 end"), shown)).unwrap();
    assert_eq!(i.callbacks, [Disposition::Replaced(consumer)]);
    assert!(noted(
        &i,
        NoteKind::Callback {
            field: "value",
            why: consumer
        }
    ));
    critical(
        import(&skin(&set("function() return ms.number(101) end"), shown)),
        "value",
        consumer,
    );
}

#[test]
fn a_function_no_consumer_reads_is_classified_only() {
    // A slider's value drops the slider before it is consumed; an
    // action, an event writer and a custom event's condition are never
    // consumed.
    let dst = "{ id = \"sl\", dst = { { w = 8, h = 8 } } },";
    for entry in [
        skin(
            "skin.slider = { { id = \"sl\", src = \"a\", x = 0, y = 0, w = 8, h = 8, type = 4, angle = 2,\n\
               value = function() return 1 end } }",
            dst,
        ),
        with_image(
            "{ id = \"sl\", src = \"a\", x = 0, y = 0, w = 8, h = 8, act = function() return 1 end }",
            dst,
        ),
        skin(
            "skin.font = { { id = \"f\", path = \"a.ttf\" } }\n\
             skin.text = { { id = \"sl\", font = \"f\", size = 10, ref = 10, event = function() return 1 end } }",
            dst,
        ),
        skin(
            "skin.customEvents = { { id = 1000, action = function() return 1 end,\n\
               condition = function() return true end } }",
            dst,
        ),
    ] {
        let body = &entry;
        let i = import(&entry).unwrap_or_else(|e| panic!("{body}: {e:?}"));
        assert!(
            !i.callbacks.is_empty()
                && i.callbacks
                    .iter()
                    .all(|d| *d == Disposition::ClassifiedOnly),
            "{body}: {:?}",
            i.callbacks
        );
    }
}

#[test]
fn the_open_site_is_counted_where_the_conversion_meets_it() {
    let entry = beside(
        "skin.text = { { id = \"t\", value = \"score\" }, { id = \"u\", value = \"title\" } }\n\
         local mixed = { function() end, k = 1 }\n\
         skin.aaa = mixed",
        "{ id = \"i\", op = mixed, dst = { { w = 6, h = 6 } } },",
    );
    let d = super::callback_tests::folder(&entry);
    let cancel = super::Cancel::new();
    let h = super::read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let facts = super::facts::ImportFacts::unavailable(skin_model::ScreenKind::Play);
    let (def, _, _, _) = h.configured(
        &crate::import::Config::defaults(h.header()),
        &facts,
        &cancel,
    );
    let def = def.unwrap();
    let an = def
        .arena
        .lua
        .as_deref()
        .and_then(|l| l.analysis.as_deref())
        .unwrap();
    assert_eq!((an.scripts, an.unread), (1, 1));
    assert!(an.open.is_some());
}
