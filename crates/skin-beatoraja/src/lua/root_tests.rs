//! Roots, inert callables and the open analysis (design.md "beatoraja
//! Lua Skins", per-frame closures): which functions and strings of the
//! returned table the source reads as code, by its schema, and what a
//! root the analysis cannot see does to the proofs that rest on captured
//! state. Each test imports through the candidate's two passes.

use super::callback_tests::{gauge, image, import, skin};
use super::{Disposition, LuaImported, LuaWhy, Unsupported};
use crate::error::ImportError;
use crate::import::NoteKind;
use crate::tree::Kind;

/// A frame-time writer: walked as a root, it opens the analysis at its
/// assignment.
const WRITER: &str = "function() t.n = 1 return true end";

/// A critical gauge `draw` reading option `x`, which the body sets to 1.
fn config_gauge() -> String {
    gauge("function() return skin_config.option[\"x\"] == 1 end")
}

/// A skin whose configured pass sets option `x` to 1, declares the
/// writer's table `t`, then runs `body`, with the critical
/// configuration reader first among the destinations and `dst` after it.
fn beside(body: &str, dst: &str) -> String {
    skin(
        &format!("skin_config.option.x = 1\nlocal t = {{}}\n{body}"),
        &format!("{}{dst}", config_gauge()),
    )
}

/// The configured pass of `entry`: its definition, converted and not
/// yet imported.
pub(super) fn configured(entry: &str) -> crate::resolve::Definition {
    let d = super::callback_tests::folder(entry);
    let cancel = super::Cancel::new();
    let h = super::read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let facts = super::facts::ImportFacts::unavailable(skin_model::ScreenKind::Play);
    let config = crate::import::Config::defaults(h.header());
    h.configured(&config, &facts, &cancel).0.unwrap()
}

/// The analysis of `def`.
pub(super) fn analysis(def: &crate::resolve::Definition) -> &super::callback::Analysis {
    def.arena
        .lua
        .as_deref()
        .and_then(|l| l.analysis.as_deref())
        .unwrap()
}

/// Checks that importing `entry` refuses its critical `draw` reader
/// because the analysis is open, located exactly at the open site: the
/// converted node of `kind` under `key` that the configured pass met
/// first.
#[track_caller]
pub(super) fn interferes(entry: &str, (kind, key): (Kind, Option<&str>)) {
    let def = configured(entry);
    let Some(super::callback::Open {
        site: super::callback::Site::Node(open),
        ..
    }) = analysis(&def).open.get()
    else {
        panic!("no open node")
    };
    assert_eq!(def.arena.node(open).kind, kind);
    let got: Option<String> = def.arena.key(open).map(Iterator::collect);
    assert_eq!(got.as_deref(), key);
    let site = def.loc(open);
    assert_eq!((site.file.as_str(), site.line), ("s.luaskin", 0));
    match import(entry) {
        Err(ImportError::Lua {
            why:
                LuaWhy::Critical {
                    field: "draw",
                    why: Unsupported::Interference,
                },
            at: Some(at),
        }) => assert_eq!(at, site),
        other => panic!("{other:?}"),
    }
}

/// A script's site: its string under `key`.
pub(super) fn script(key: &str) -> (Kind, Option<&str>) {
    (Kind::Lua, Some(key))
}

/// Checks that `r` imported with the critical reader lowered.
#[track_caller]
pub(super) fn lowers(r: Result<LuaImported, ImportError>) -> LuaImported {
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
    // root: walked, and the critical reader refuses at its assignment.
    for key in ["aaa", "zzz"] {
        let entry = beside(
            &format!("local w = {WRITER}\nskin.{key} = {{ w }}"),
            &image("w"),
        );
        assert_eq!(
            super::tests::why(import(&entry)),
            LuaWhy::Critical {
                field: "draw",
                why: Unsupported::Interference
            },
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
    // destination under one: each writer is walked, a writer root.
    for dst in [
        format!("{{ id = \"i\", op = {{ hidden = {WRITER} }}, dst = {{ {{ w = 6, h = 6 }} }} }},"),
        format!("extra = {{ id = \"i\", draw = {WRITER}, dst = {{ {{ w = 6, h = 6 }} }} }},"),
    ] {
        let entry = beside("", &dst);
        let def = configured(&entry);
        super::callback::admit(&def.arena).unwrap();
        let an = analysis(&def);
        assert!(an.open.get().is_some(), "{dst}");
        assert_eq!(an.writers.borrow().len(), 1, "{dst}");
    }
    // The mixed `op` table's entry, consumed: the critical reader beside
    // it refuses at the write.
    let entry = beside(
        "",
        &format!("{{ id = \"i\", op = {{ hidden = {WRITER} }}, dst = {{ {{ w = 6, h = 6 }} }} }},"),
    );
    assert_eq!(
        super::tests::why(import(&entry)),
        LuaWhy::Critical {
            field: "draw",
            why: Unsupported::Interference
        }
    );
}

#[test]
fn a_root_the_analysis_cannot_see_voids_the_capture_proof() {
    for (body, dst, site) in [
        // Scripts: a `draw` and a `timer` the source compiles.
        (
            "",
            image("\"(function() skin_config.option.x = 2 return true end)()\""),
            script("draw"),
        ),
        (
            "",
            "{ id = \"i\", timer = \"(function() skin_config.option.x = 2 return 0 end)()\", \
             dst = { { w = 6, h = 6 } } },"
                .to_owned(),
            script("timer"),
        ),
        // A string property's and an integer property's names that their
        // own factories do not resolve.
        (
            "skin.text = { { id = \"t\", value = \"score\" } }",
            String::new(),
            script("value"),
        ),
        (
            "skin.value = { { id = \"v\", value = \"ranking_exscore+1\" } }",
            String::new(),
            script("value"),
        ),
        // A mixed `op` list whose positional entry holds a function.
        (
            "",
            format!(
                "{{ id = \"i\", op = {{ {WRITER}, k = 1 }}, dst = {{ {{ w = 6, h = 6 }} }} }},"
            ),
            (Kind::Mixed, Some("op")),
        ),
    ] {
        interferes(&beside(body, &dst), site);
    }
    // The table met first at an ignored key and then at an `op`, or the
    // other way round: its unread entry is seen at the `op` either way.
    for key in ["aaa", "zzz"] {
        let body = format!(
            "local conditions = {{ {WRITER}, keep = function() return true end }}\n\
             skin.{key} = conditions"
        );
        let dst = "{ id = \"i\", op = conditions, dst = { { w = 6, h = 6 } } },";
        interferes(&beside(&body, dst), (Kind::Mixed, Some("op")));
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
    interferes(&entry, script("draw"));
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
    interferes(
        &beside(&font(&format!("{{ {meta} }}")), ""),
        (Kind::Empty, None),
    );
    lowers(import(&beside(&font("{ { path = \"a\" } }"), "")));
    // A mixed fallback list whose positional entry is that table: unread,
    // whether met there directly or first through an ignored key.
    let mixed = format!("local hidden = {meta}\nlocal fb = {{ hidden, keep = \"a.ttf\" }}\n");
    let listed = (Kind::Mixed, Some("fallback"));
    interferes(&beside(&format!("{mixed}{}", font("fb")), ""), listed);
    for key in ["aaa", "zzz"] {
        let body = format!("{mixed}skin.{key} = fb\n{}", font("fb"));
        interferes(&beside(&body, ""), listed);
    }
}

/// Whether `i` was imported with a note of kind `kind`.
fn noted(i: &LuaImported, kind: NoteKind) -> bool {
    i.imported.report.notes.iter().any(|n| n.kind == kind)
}

/// Checks that `r` refuses the critical callable in `field` for `why`.
#[track_caller]
fn critical(r: Result<LuaImported, ImportError>, field: &'static str, why: Unsupported) {
    assert_eq!(super::tests::why(r), LuaWhy::Critical { field, why });
}

/// [`skin`] with `image` added to its images.
fn with_image(image: &str, dst: &str) -> String {
    skin(&format!("local more = {image}"), dst).replace(
        "skin.gauge = ",
        "skin.image[#skin.image + 1] = more\nskin.gauge = ",
    )
}

/// The read of an unmapped value property.
const UNMAPPED: &str = "function() return ms.number(426) > 0 end";

#[test]
fn an_op_function_is_a_condition_by_its_element_s_role() {
    let op = |id: &str, f: &str| {
        format!("{{ id = \"{id}\", op = {{ {f} }}, dst = {{ {{ w = 64, h = 10 }} }} }},")
    };
    let i = import(&skin("", &op("i", "function() return true end"))).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered]);
    let why = Unsupported::Unmapped { api: 1, id: 426 };
    let i = import(&skin("", &op("i", UNMAPPED))).unwrap();
    assert_eq!(i.callbacks, [Disposition::Replaced(why)]);
    assert!(noted(&i, NoteKind::Callback { field: "op", why }));
    critical(import(&skin("", &op("g", UNMAPPED))), "op", why);
}

#[test]
fn a_timer_function_is_consumed_only_at_a_destination() {
    let consumer = Unsupported::Consumer("timer");
    let timed = |id: &str| {
        format!(
            "{{ id = \"{id}\", timer = function() return 0 end, dst = {{ {{ w = 64, h = 10 }} }} }},"
        )
    };
    // A destination's: the program's timer callback, decorative or
    // critical.
    for id in ["i", "g"] {
        let i = import(&skin("", &timed(id))).unwrap();
        assert_eq!(i.callbacks, [Disposition::Compiled], "{id}");
    }
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
    // An image set's value that does not lower: by its role, which its
    // reads also give, its reason kept, never its `ref`.
    let set = |f: &str| {
        format!(
            "skin.imageset = {{ {{ id = \"s\", ref = 10, images = {{ \"i\", \"i\" }}, value = {f} }} }}"
        )
    };
    let why = Unsupported::Type;
    let shown = "{ id = \"s\", dst = { { w = 64, h = 64 } } },";
    let i = import(&skin(&set("function() return \"x\" end"), shown)).unwrap();
    assert_eq!(i.callbacks, [Disposition::Replaced(why)]);
    assert!(noted(
        &i,
        NoteKind::Callback {
            field: "value",
            why
        }
    ));
    critical(
        import(&skin(
            &set("function() local n = ms.number(101) return \"x\" end"),
            shown,
        )),
        "value",
        why,
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
    let def = configured(&entry);
    let an = analysis(&def);
    assert_eq!((an.scripts, an.unread), (1, 1));
    assert!(an.open.get().is_some());
}

/// `critical`, a critical gauge `draw`, beside a decorative `draw` script
/// that opens the analysis, after `body`.
pub(super) fn opened(body: &str, critical: &str) -> String {
    skin(
        body,
        &format!(
            "{}{}",
            gauge(critical),
            image("\"(function() return true end)()\"")
        ),
    )
}

#[test]
fn an_open_analysis_proves_no_fold_and_no_binding() {
    // A captured scalar, `timer_off_value` and a helper: each lowers
    // while the analysis is closed, and refuses the critical reader at
    // the script while it is open.
    for (body, critical) in [
        ("local on = true", "function() return on end"),
        (
            "",
            "function() return ms.timer_off_value == ms.timer(41) end",
        ),
        (
            "local function h() return true end",
            "function() return h() end",
        ),
    ] {
        lowers(import(&skin(body, &gauge(critical))));
        interferes(&opened(body, critical), script("draw"));
    }
}

#[test]
fn an_open_analysis_replaces_a_decorative_reader() {
    // A decorative reader is replaced, reported.
    let entry = skin(
        "skin_config.option.x = 1",
        &format!(
            "{}{}",
            image("function() return skin_config.option[\"x\"] == 1 end"),
            image("\"(function() return true end)()\"")
        ),
    );
    let i = import(&entry).unwrap();
    let why = Unsupported::Interference;
    assert_eq!(i.callbacks, [Disposition::Replaced(why)]);
    assert!(noted(&i, NoteKind::Callback { field: "draw", why }));
}

#[test]
fn an_open_analysis_still_lowers_literals() {
    // One of literals alone still lowers.
    let i = import(&opened("", "function() local a = 1 return a + 1 > 1 end")).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered]);
}

#[test]
fn op_functions_join_in_the_source_s_order() {
    use crate::import::ops::{Cond, named, numeric};
    use skin_model::{PLAY, Scope, State, Ty, compile};
    let expr = |c: Option<Cond>| match c {
        Some(Cond::Expr { src, .. }) => src,
        other => panic!("{other:?}"),
    };
    let none = std::collections::BTreeMap::new();
    let name = crate::import::ops::KNOWN_BOOLEAN_IDS
        .iter()
        .find(|(id, _)| *id == 272)
        .map(|&(_, n)| n)
        .unwrap();
    // A function, a name and an id in the list, and a `draw`.
    let entry = skin(
        "",
        &format!(
            "{{ id = \"i\", op = {{ function() return ms.option(273) end, \"{name}\", 271 }},\n\
             draw = function() return ms.option(270) end, dst = {{ {{ w = 64, h = 64 }} }} }},"
        ),
    );
    crate::import::VISIBLE.with(|v| v.borrow_mut().clear());
    let i = import(&entry).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered, Disposition::Lowered]);
    let visible = crate::import::VISIBLE.with(|v| v.borrow().clone());
    let [Some(visible)] = visible.as_slice() else {
        panic!("{visible:?}")
    };
    // The id first, then the function and the name in the list's order,
    // then `draw`, each function's read wrapped as it is lowered.
    let want = format!(
        "({}) && (({})) && ({}) && (({}))",
        expr(Some(numeric(271, &none))),
        expr(Some(numeric(273, &none))),
        expr(named(name)),
        expr(Some(numeric(270, &none))),
    );
    let want = compile(&want, &Scope::new(&PLAY), Ty::Bool).unwrap();
    assert_eq!(*visible, want);
    // Evaluated, it is the conjunction of the four.
    let timers = vec![-1_i32; PLAY.timers.len()];
    for v in [0, 1] {
        let fields = vec![v; PLAY.fields.len()];
        let state = State {
            fields: &fields,
            timers: &timers,
        };
        assert_eq!(visible.eval_bool(&state), want.eval_bool(&state));
    }
}
