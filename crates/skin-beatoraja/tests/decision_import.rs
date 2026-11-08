//! The decision importer (docs/stage3.md §3, Decision; S10-09f): a
//! decision skin's ids through the decision profile, JSON and Lua alike,
//! into `build_decision` frames: the chart's labels (C2), the option
//! constants (C3), the declared timing and its clamp (C5), and what the
//! decision does not show, dropped or ignored with a note (C6).

mod common;

use bms_model::TimeUs;
use common::decision::{
    Dec, folder, import_decision, lua_decision, lua_folder, lua_folder_with, lua_outcome,
};
use skin_beatoraja::{
    Fact, FactId, ImportError, ImportFacts, LuaOutcome, NoteKind, Object, PropertyKind, Report,
    Severity,
};
use skin_model::{DecisionTiming, MAX_DECISION_PHASE, ScreenKind, Skin};
use tempfile::TempDir;

/// The top of a quad drawn at `y` with height `h`.
fn top(y: i32, h: i32) -> f32 {
    (1_080 - y - h) as f32
}

/// The dropped and ignored notes of a report, by kind.
fn notes(r: &Report) -> Vec<(Severity, NoteKind)> {
    let mut n: Vec<_> = r
        .notes
        .iter()
        .filter(|n| matches!(n.severity, Severity::Dropped | Severity::Ignored))
        .map(|n| (n.severity, n.kind))
        .collect();
    n.sort_by_key(|n| format!("{n:?}"));
    n
}

/// A JSON skin and a Lua skin of the same elements, both imported: their
/// skins, reports and folders (kept while the skin's atlas reads them).
fn both(json_body: &str, lua_body: &str) -> [(Skin, Report, TempDir); 2] {
    both_with(json_body, "", lua_body)
}

/// [`both`] with the Lua skin's `header` fields.
fn both_with(json_body: &str, header: &str, lua_body: &str) -> [(Skin, Report, TempDir); 2] {
    let j = folder(json_body);
    let json = import_decision(j.path()).unwrap_or_else(|e| panic!("json: {e}"));
    let l = lua_folder_with(header, "", lua_body);
    let lua = lua_decision(l.path()).unwrap_or_else(|e| panic!("lua: {e}"));
    [
        (json.skin, json.report, j),
        (lua.imported.skin, lua.imported.report, l),
    ]
}

/// The text ids of C2: the labels 10 to 16, then the table's line.
const TEXT_IDS: [i32; 8] = [10, 11, 12, 13, 14, 15, 16, 1_003];

/// C2: texts 10 to 16 show the chosen chart's labels as play builds them
/// (12 and 16 joined), through the outline lines the skin requests and
/// the bitmap font's layout, JSON by `ref` and Lua by `ref` or a
/// `main_state.text` read alike; 1003 drops with a note.
#[test]
fn decision_texts_show_the_chart_labels() {
    let mut texts = Vec::new();
    let mut dsts = Vec::new();
    let mut lua_texts = Vec::new();
    let mut lua_dsts = Vec::new();
    for (i, id) in TEXT_IDS.iter().enumerate() {
        let y = 100 + 20 * i32::try_from(i).unwrap();
        texts.push(format!(
            r#"{{"id": "t{id}", "font": "f", "size": 8, "ref": {id}}}"#
        ));
        dsts.push(format!(
            r#"{{"id": "t{id}", "dst": [{{"x": 200, "y": {y}, "w": 300, "h": 8}}]}}"#
        ));
        // The odd labels read through a callback, the rest by `ref`.
        let source = if id % 2 == 1 && *id < 1_000 {
            format!("value = function() return ms.text({id}) end")
        } else {
            format!("ref = {id}")
        };
        lua_texts.push(format!(
            "{{ id = \"t{id}\", font = \"f\", size = 8, {source} }}"
        ));
        lua_dsts.push(format!(
            "{{ id = \"t{id}\", dst = {{ {{ x = 200, y = {y}, w = 300, h = 8 }} }} }}"
        ));
    }
    // The bitmap font's title.
    texts.push(r#"{"id": "bt", "font": "g", "size": 8, "ref": 10}"#.to_owned());
    dsts.push(r#"{"id": "bt", "dst": [{"x": 200, "y": 400, "w": 300, "h": 8}]}"#.to_owned());
    lua_texts.push("{ id = \"bt\", font = \"g\", size = 8, ref = 10 }".to_owned());
    lua_dsts.push("{ id = \"bt\", dst = { { x = 200, y = 400, w = 300, h = 8 } } }".to_owned());
    let json = format!(
        r#""text": [{}], "destination": [{}]"#,
        texts.join(", "),
        dsts.join(", ")
    );
    let lua = format!(
        "skin.text = {{ {} }}\nskin.destination = {{ {} }}",
        lua_texts.join(", "),
        lua_dsts.join(", ")
    );
    let dropped = vec![(
        Severity::Dropped,
        NoteKind::UnsupportedProperty {
            kind: PropertyKind::String,
            id: Some(1_003),
        },
    )];
    for (path, (skin, report, _dir)) in ["json", "lua"].into_iter().zip(both(&json, &lua)) {
        assert_eq!(notes(&report), dropped, "{path}");
        let mut d = Dec::new(skin);
        let want: Vec<(u16, String)> = ["AB", "B", "AB B", "AA", "A", "BB", "A BB"]
            .iter()
            .zip(0..)
            .map(|(t, slot)| (slot, (*t).to_owned()))
            .collect();
        assert_eq!(d.requests(), want, "{path}");
        // Nothing draws text before its lines are attached.
        assert!(d.frame(0).is_empty(), "{path}");
        d.attach_lines();
        assert_eq!(d.frame(0).len(), 7, "{path}: one quad per outline line");
        d.skin.attach_decision_bitmap_text(&mut d.dec);
        let quads = d.frame(0);
        assert_eq!(quads.len(), 9, "{path}: and the bitmap title's two glyphs");
    }
}

/// The options of C3 and whether an element under each draws.
const OPTIONS: &[(i32, bool)] = &[
    (32, false),
    (-32, true),
    (33, false),
    (-33, true),
    (50, true),
    (-50, false),
    (51, false),
    (-51, true),
    (160, true),
    (-160, false),
    (161, false),
    (164, false),
    (-164, true),
    (280, false),
    (283, false),
    (289, false),
    (290, false),
    (-290, true),
    (1_008, false),
    (-1_008, true),
    // The configured option, on by default.
    (901, true),
    (-901, false),
];

/// The configured option of [`OPTIONS`], declared in JSON.
const PROPERTY: &str =
    r#""property": [{"name": "mode", "item": [{"name": "on", "op": 901}], "def": "on"}],"#;

/// The same option declared in Lua.
const LUA_PROPERTY: &str =
    "property = { { name = \"mode\", item = { { name = \"on\", op = 901 } }, def = \"on\" } },";

/// C3: options 32, 33, 51, 161-164, 280-283, 289, 290 and 1008 are false
/// and 50 and 160 true on the decision screen, their negations the
/// reverse, JSON by `op` and Lua by a `draw` callback's
/// `main_state.option` read alike; a configured option keeps its value.
#[test]
fn decision_options_are_the_screen_s_constants() {
    let y = |i: usize| 100 + 10 * i32::try_from(i).unwrap();
    let json_dsts: Vec<String> = OPTIONS
        .iter()
        .enumerate()
        .map(|(i, (op, _))| {
            format!(
                r#"{{"id": "dot", "op": [{op}], "dst": [{{"x": 0, "y": {}, "w": 8, "h": 8}}]}}"#,
                y(i)
            )
        })
        .collect();
    let lua_dsts: Vec<String> = OPTIONS
        .iter()
        .enumerate()
        .map(|(i, &(op, _))| {
            // A callback's `main_state.option` reads the built-ins; the
            // configured option is the skin's own `op`.
            let cond = match op {
                901 | -901 => format!("op = {{ {op} }}"),
                _ if op < 0 => format!("draw = function() return not ms.option({}) end", -op),
                _ => format!("draw = function() return ms.option({op}) end"),
            };
            format!(
                "{{ id = \"dot\", {cond}, dst = {{ {{ x = 0, y = {}, w = 8, h = 8 }} }} }}",
                y(i)
            )
        })
        .collect();
    let json = format!(r#"{PROPERTY} "destination": [{}]"#, json_dsts.join(", "));
    let lua = format!("skin.destination = {{ {} }}", lua_dsts.join(", "));
    let mut want: Vec<f32> = OPTIONS
        .iter()
        .enumerate()
        .filter(|(_, (_, on))| *on)
        .map(|(i, _)| top(y(i), 8))
        .collect();
    want.sort_by(f32::total_cmp);
    for (path, (skin, report, _dir)) in
        ["json", "lua"]
            .into_iter()
            .zip(both_with(&json, LUA_PROPERTY, &lua))
    {
        assert_eq!(Dec::new(skin).tops(0), want, "{path}");
        // A folded condition known false is noted as ignored, nothing
        // else.
        assert!(
            notes(&report)
                .iter()
                .all(|n| *n == (Severity::Ignored, NoteKind::Ignored { field: "op" })),
            "{path}: {:?}",
            notes(&report)
        );
    }
}

/// C3: a built-in option outside the decision profile is unsupported,
/// never a constant: the judgment class (a load-time fact only) and the
/// stage file, banner and back image (190-195, until S10-12); an element
/// under one is dropped with a note, JSON and Lua alike.
#[test]
fn other_built_in_options_are_unsupported() {
    for id in [180, 184, 190, 195] {
        let json = format!(
            r#""destination": [{{"id": "dot", "op": [{id}], "dst": [{{"x": 0, "y": 100, "w": 8, "h": 8}}]}},
                {{"id": "lab", "dst": [{{"x": 0, "y": 200, "w": 8, "h": 8}}]}}]"#
        );
        let lua = format!(
            "skin.destination = {{ {{ id = \"dot\", draw = function() return ms.option({id}) end, \
             dst = {{ {{ x = 0, y = 100, w = 8, h = 8 }} }} }}, \
             {{ id = \"lab\", dst = {{ {{ x = 0, y = 200, w = 8, h = 8 }} }} }} }}"
        );
        for (path, (skin, report, _dir)) in ["json", "lua"].into_iter().zip(both(&json, &lua)) {
            let unsupported = NoteKind::UnsupportedProperty {
                kind: PropertyKind::Boolean,
                id: Some(id),
            };
            assert!(
                report.notes.iter().any(|n| n.kind == unsupported),
                "{path} {id}: {:?}",
                report.notes
            );
            assert_eq!(Dec::new(skin).tops(0), [top(200, 8)], "{path} {id}");
        }
    }
}

/// C3: a Lua skin's reads while it loads go through the same profile:
/// the constants answer at once and record nothing, the judgment class is
/// pending in the base snapshot, and any other built-in is live state,
/// which refuses the pass.
#[test]
fn a_lua_decision_skin_reads_the_constants_while_it_loads() {
    let pre = "local x = 0\n\
               if ms.option(50) and not ms.option(51) and ms.option(160) \
               and not ms.option(161) and not ms.option(32) and not ms.option(33) \
               and not ms.option(290) and ms.option(-290) and not ms.option(1008) \
               and not ms.option(283) and not ms.option(289) then x = 300 end";
    let body =
        "skin.destination = { { id = \"dot\", dst = { { x = x, y = 100, w = 8, h = 8 } } } }";
    let d = lua_folder(pre, body);
    let i = lua_decision(d.path()).unwrap_or_else(|e| panic!("{e}"));
    assert!(i.facts.reads.is_empty(), "{:?}", i.facts.reads);
    let mut dec = Dec::new(i.imported.skin);
    let quads = dec.frame(0);
    assert_eq!(quads.len(), 1);
    assert_eq!(quads[0].rect[0], 300.0);
    // The judgment class is known only at the choice.
    let d = lua_folder("local c = ms.option(180)", body);
    let base = ImportFacts::base(ScreenKind::Decision, Fact::Known(false));
    match lua_outcome(d.path(), &base) {
        Ok(LuaOutcome::Pending(p)) => assert_eq!(p.pending, FactId::JudgeClass),
        other => panic!("{other:?}"),
    }
    // A built-in outside the profile is live state.
    let d = lua_folder("local c = ms.option(190)", body);
    let r = lua_outcome(d.path(), &base);
    assert!(r.is_err(), "{:?}", r.map(|_| ()));
}

/// A decision skin's declared timing from the top-level fields `top`
/// (`"name": value, ...`, ending with a comma) in JSON, and from `lua`'s
/// statements in Lua: each timing and report.
fn timing(top: &str, lua: &str) -> [(DecisionTiming, Report); 2] {
    let body = format!(r#"{top} "destination": [{{"id": "dot", "dst": [{{"w": 8, "h": 8}}]}}]"#);
    let lua_body = format!(
        "{lua}\nskin.destination = {{ {{ id = \"dot\", dst = {{ {{ w = 8, h = 8 }} }} }} }}"
    );
    both(&body, &lua_body).map(|(s, r, _)| (s.decision_timing(), r))
}

/// The clamp notes of `r`.
fn clamps(r: &Report) -> Vec<(Severity, NoteKind)> {
    r.notes
        .iter()
        .filter(|n| matches!(n.kind, NoteKind::DecisionClamped { .. }))
        .map(|n| (n.severity, n.kind))
        .collect()
}

/// C5: a decision skin's `scene`, `fadeout` and `input` are read in µs
/// from JSON and Lua alike: absent 0, negative 0, and a `scene` or
/// `fadeout` past 10 s clamped to it with a deviation note naming the
/// field and the declaration; `input` is kept as declared.
#[test]
fn a_decision_skin_declares_its_timing_clamped() {
    let ms = |n: i64| TimeUs(n * 1_000);
    let t = |scene, fadeout, input| DecisionTiming {
        scene,
        fadeout,
        input,
    };
    let clamped = |field, declared_ms| {
        (
            Severity::Deviation,
            NoteKind::DecisionClamped { field, declared_ms },
        )
    };
    let max = i32::MAX;
    for (json, lua, want, notes) in [
        ("", "", t(ms(0), ms(0), ms(0)), vec![]),
        (
            r#""scene": 2500, "fadeout": 500, "input": 500,"#,
            "skin.scene = 2500 skin.fadeout = 500 skin.input = 500",
            t(ms(2_500), ms(500), ms(500)),
            vec![],
        ),
        (
            r#""scene": -1, "fadeout": -5, "input": -9,"#,
            "skin.scene = -1 skin.fadeout = -5 skin.input = -9",
            t(ms(0), ms(0), ms(0)),
            vec![],
        ),
        (
            r#""scene": 10000, "fadeout": 10000, "input": 20000,"#,
            "skin.scene = 10000 skin.fadeout = 10000 skin.input = 20000",
            t(ms(10_000), ms(10_000), ms(20_000)),
            vec![],
        ),
        (
            r#""scene": 10001, "fadeout": 400,"#,
            "skin.scene = 10001 skin.fadeout = 400",
            t(ms(10_000), ms(400), ms(0)),
            vec![clamped("scene", 10_001)],
        ),
        (
            r#""scene": 300, "fadeout": 10001,"#,
            "skin.scene = 300 skin.fadeout = 10001",
            t(ms(300), ms(10_000), ms(0)),
            vec![clamped("fadeout", 10_001)],
        ),
        (
            &format!(r#""scene": {max}, "fadeout": {max},"#),
            &format!("skin.scene = {max} skin.fadeout = {max}"),
            t(ms(10_000), ms(10_000), ms(0)),
            vec![clamped("fadeout", max), clamped("scene", max)],
        ),
    ] {
        for (path, (got, report)) in ["json", "lua"].into_iter().zip(timing(json, lua)) {
            assert_eq!(got, want, "{path}: {json}");
            let mut c = clamps(&report);
            c.sort_by_key(|n| format!("{n:?}"));
            assert_eq!(c, notes, "{path}: {json}");
        }
    }
    assert_eq!(MAX_DECISION_PHASE, ms(10_000));
    // A field the integer reader refuses refuses the import.
    let d = folder(r#""scene": "abc", "destination": []"#);
    assert!(matches!(
        import_decision(d.path()),
        Err(ImportError::Type { .. })
    ));
}

/// C6 and the timers: a number, a graph and a slider are dropped with a
/// note, an image's `act` is ignored with a note, an image set choosing by
/// a known image index is dropped and one with no index shows its first
/// image; timer 2 is the fade-out, timers 0 and 1 never start and play's
/// timers are unsupported. JSON and Lua give the same notes and frames.
#[test]
fn what_the_decision_does_not_show_is_noted() {
    let json = r#""value": [{"id": "ex", "src": "s", "x": 0, "y": 32, "w": 40, "h": 8, "divx": 10, "digit": 4, "ref": 71}],
        "judgegraph": [{"id": "jg", "type": 0}],
        "judge": [{"id": "jd"}],
        "slider": [{"id": "sl", "src": "s", "x": 0, "y": 0, "w": 8, "h": 8, "angle": 0, "range": 10, "type": 0}],
        "imageset": [{"id": "clr", "ref": 370, "images": ["b0", "b1"]},
                     {"id": "set", "images": ["b5", "b1"]},
                     {"id": "acted", "act": 15, "images": ["lab"]}],
        "destination": [
            {"id": "ex", "dst": [{"x": 0, "y": 100, "w": 4, "h": 8}]},
            {"id": "jg", "dst": [{"x": 0, "y": 110, "w": 8, "h": 8}]},
            {"id": "sl", "dst": [{"x": 0, "y": 120, "w": 8, "h": 8}]},
            {"id": "clr", "dst": [{"x": 0, "y": 130, "w": 8, "h": 8}]},
            {"id": "set", "dst": [{"x": 0, "y": 140, "w": 8, "h": 8}]},
            {"id": "acted", "dst": [{"x": 0, "y": 150, "w": 8, "h": 8}]},
            {"id": "bar", "timer": 2, "dst": [{"x": 0, "y": 160, "w": 8, "h": 8}]},
            {"id": "b0", "timer": 1, "dst": [{"x": 0, "y": 170, "w": 8, "h": 8}]},
            {"id": "b1", "timer": 0, "dst": [{"x": 0, "y": 180, "w": 8, "h": 8}]},
            {"id": "lamp", "timer": 41, "dst": [{"x": 0, "y": 190, "w": 8, "h": 8}]},
            {"id": "dot", "timer": 2, "dst": [{"x": 0, "y": 200, "w": 8, "h": 8}]},
            {"id": "jd", "dst": [{"x": 0, "y": 210, "w": 8, "h": 8}]}]"#;
    let lua = "skin.value = { { id = \"ex\", src = \"s\", x = 0, y = 32, w = 40, h = 8, divx = 10, digit = 4, \
                 value = function() return ms.number(71) end } }\n\
               skin.judgegraph = { { id = \"jg\", type = 0 } }\n\
               skin.judge = { { id = \"jd\" } }\n\
               skin.slider = { { id = \"sl\", src = \"s\", x = 0, y = 0, w = 8, h = 8, angle = 0, range = 10, type = 0 } }\n\
               skin.imageset = { { id = \"clr\", value = function() return ms.event_index(370) end, images = { \"b0\", \"b1\" } }, \
                 { id = \"set\", images = { \"b5\", \"b1\" } }, \
                 { id = \"acted\", act = 15, images = { \"lab\" } } }\n\
               skin.destination = {\n\
                 { id = \"ex\", dst = { { x = 0, y = 100, w = 4, h = 8 } } },\n\
                 { id = \"jg\", dst = { { x = 0, y = 110, w = 8, h = 8 } } },\n\
                 { id = \"sl\", dst = { { x = 0, y = 120, w = 8, h = 8 } } },\n\
                 { id = \"clr\", dst = { { x = 0, y = 130, w = 8, h = 8 } } },\n\
                 { id = \"set\", dst = { { x = 0, y = 140, w = 8, h = 8 } } },\n\
                 { id = \"acted\", dst = { { x = 0, y = 150, w = 8, h = 8 } } },\n\
                 { id = \"bar\", timer = 2, dst = { { x = 0, y = 160, w = 8, h = 8 } } },\n\
                 { id = \"b0\", timer = 1, dst = { { x = 0, y = 170, w = 8, h = 8 } } },\n\
                 { id = \"b1\", timer = 0, dst = { { x = 0, y = 180, w = 8, h = 8 } } },\n\
                 { id = \"lamp\", timer = 41, dst = { { x = 0, y = 190, w = 8, h = 8 } } },\n\
                 { id = \"dot\", draw = function() return ms.timer(2) ~= ms.timer_off_value end, \
                   dst = { { x = 0, y = 200, w = 8, h = 8 } } },\n\
                 { id = \"jd\", dst = { { x = 0, y = 210, w = 8, h = 8 } } } }";
    let [(json_skin, json_report, _j), (lua_skin, lua_report, _l)] = both(json, lua);
    let dropped = |object| (Severity::Dropped, NoteKind::Unsupported { object });
    let n = notes(&json_report);
    for want in [
        dropped(Object::Value),
        dropped(Object::JudgeGraph),
        dropped(Object::Slider),
        (Severity::Ignored, NoteKind::Ignored { field: "act" }),
        // Play's own objects are not looked up on the decision screen.
        (Severity::Ignored, NoteKind::UndefinedObject),
    ] {
        assert!(n.contains(&want), "{want:?} in {n:?}");
    }
    let mut frames = Vec::new();
    for skin in [json_skin, lua_skin] {
        let mut d = Dec::new(skin);
        let before = d.drawn(0);
        d.timers.fadeout = Some(TimeUs(0));
        let after = d.drawn(0);
        frames.push((before, after));
    }
    let (before, after) = &frames[0];
    // The set with no index shows its first image; the act's image draws.
    let b5 = ([20_u8, 4_u8], top(140, 8));
    let lab = ([36, 4], top(150, 8));
    let bar = ([44, 4], top(160, 8));
    assert_eq!(before, &vec![b5, lab]);
    // The fade-out's timer, read by `timer` in JSON and by a callback's
    // `main_state.timer(2)` in Lua.
    let dot = ([52, 4], top(200, 8));
    assert_eq!(after, &vec![b5, lab, bar, dot]);
    assert_eq!(frames[0], frames[1], "json and lua draw alike");
    // The image index 370 is noted by each path's own reader: JSON's `ref`
    // as the property, Lua's `event_index` read as its callback (the
    // shared callback path names only numbers and options by property).
    let index = |r: &Report| {
        let (i, rest): (Vec<_>, Vec<_>) = notes(r).into_iter().partition(|(_, k)| {
            matches!(
                k,
                NoteKind::UnsupportedProperty {
                    kind: PropertyKind::ImageIndex,
                    id: Some(370),
                } | NoteKind::Callback {
                    field: "value",
                    why: skin_beatoraja::lua::Unsupported::Unmapped { id: 370, .. },
                }
            )
        });
        (i.len(), rest)
    };
    let (json_index, json_rest) = index(&json_report);
    let (lua_index, lua_rest) = index(&lua_report);
    assert_eq!((json_index, lua_index), (1, 1));
    assert_eq!(json_rest, lua_rest);
}
