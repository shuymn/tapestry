//! The result importer (docs/stage3.md §3, Result; S10-09g): a result
//! skin's ids through the result profile, JSON and Lua alike, into
//! `build_result` frames under shown, hidden and unavailable values (C15);
//! its essential parts (C16); and the values a Lua skin reads while it
//! loads (C17).

mod common;

use bms_model::TimeUs;
use common::result::{
    Res, best, facts, folder, import_result, lua_outcome, lua_result, ready, sheet, unavailable,
};
use common::selection::{B0, B1, BAR, DIGITS, DOT, LABEL as LAB, LAMP};
use skin_beatoraja::{
    EssentialPart, Fact, FactId, FactRead, FactValue, ImportError, ImportFacts, LuaOutcome, LuaWhy,
    NoteKind, Object, Report, Severity,
};
use skin_model::{BestView, MAX_RESULT_OUTRO, ResultNumber, ResultTiming, ResultView, ScreenKind};

/// The top of a quad drawn from a source rect at `y` of height `h`.
fn top(y: f32, h: f32) -> f32 {
    1_080.0 - y - h
}

/// The digit rows' tops.
const EX: f32 = 1_080.0 - 100.0 - 8.0;
const EX3: f32 = 1_080.0 - 150.0 - 8.0;
const BPD: f32 = 1_080.0 - 200.0 - 8.0;
const LV: f32 = 1_080.0 - 250.0 - 8.0;
const NR: f32 = 1_080.0 - 260.0 - 8.0;
const MN: f32 = 1_080.0 - 270.0 - 8.0;

/// The image set of the clear type: entry `k` is the clear type `k`'s.
const CLEAR_SET: &str = r#"["b0", "b1", "b5", "lamp", "lab", "bar"]"#;

/// The JSON skin: numbers by id (71, 178, 96, 154) and by name (`score3`,
/// 171), an image under option 90, one under `draw` by name
/// (`update_score`, 330), an image set by image index 370 and one by 451,
/// images on the fade-out and the IR timer, and a gauge graph.
fn json_body() -> String {
    format!(
        r#""imageset": [{{"id": "clr", "ref": 370, "images": {CLEAR_SET}}},
                        {{"id": "lane", "ref": 451, "images": ["b0", "b1"]}}],
           "value": [{{"id": "ex", {DIGITS}, "digit": 4, "ref": 71}},
                     {{"id": "ex3", {DIGITS}, "digit": 4, "value": "score3"}},
                     {{"id": "bpd", {DIGITS}, "digit": 4, "ref": 178}},
                     {{"id": "lv", {DIGITS}, "digit": 2, "ref": 96}},
                     {{"id": "nr", {DIGITS}, "digit": 4, "ref": 154}},
                     {{"id": "mn", {DIGITS}, "digit": 3, "ref": 374}}],
           "gaugegraph": [{{"id": "gg"}}],
           "destination": [
               {{"id": "ex", "dst": [{{"x": 0, "y": 100, "w": 4, "h": 8}}]}},
               {{"id": "ex3", "dst": [{{"x": 0, "y": 150, "w": 4, "h": 8}}]}},
               {{"id": "bpd", "dst": [{{"x": 0, "y": 200, "w": 4, "h": 8}}]}},
               {{"id": "lv", "dst": [{{"x": 0, "y": 250, "w": 4, "h": 8}}]}},
               {{"id": "nr", "dst": [{{"x": 0, "y": 260, "w": 4, "h": 8}}]}},
               {{"id": "mn", "dst": [{{"x": 0, "y": 270, "w": 4, "h": 8}}]}},
               {{"id": "dot", "op": [90], "dst": [{{"x": 0, "y": 300, "w": 8, "h": 8}}]}},
               {{"id": "lab", "draw": "update_score", "dst": [{{"x": 0, "y": 400, "w": 8, "h": 8}}]}},
               {{"id": "clr", "dst": [{{"x": 0, "y": 500, "w": 8, "h": 8}}]}},
               {{"id": "lane", "dst": [{{"x": 0, "y": 550, "w": 8, "h": 8}}]}},
               {{"id": "bar", "timer": 2, "dst": [{{"x": 0, "y": 700, "w": 8, "h": 8}}]}},
               {{"id": "lamp", "timer": 173, "dst": [{{"x": 0, "y": 750, "w": 8, "h": 8}}]}},
               {{"id": "gg", "dst": [{{"x": 0, "y": 800, "w": 8, "h": 8}}]}}]"#
    )
}

/// The selection fixtures' images as a Lua table's entries.
fn lua_images() -> String {
    [
        ("b0", 0),
        ("b1", 8),
        ("b5", 16),
        ("lamp", 24),
        ("lab", 32),
        ("bar", 40),
        ("dot", 48),
    ]
    .iter()
    .map(|(id, x)| format!("{{ id = \"{id}\", src = \"s\", x = {x}, y = 0, w = 8, h = 8 }}"))
    .collect::<Vec<_>>()
    .join(", ")
}

/// A Lua result skin: `pre` statements after the images, then `values`,
/// `sets` and `dsts` as the tables' entries.
fn lua_skin(pre: &str, values: &str, sets: &str, dsts: &str) -> String {
    format!(
        "local skin = {{ type = 7, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"s\", path = \"s.png\" }} }}\n\
         skin.font = {{ {{ id = \"f\", path = \"f.ttf\" }} }}\n\
         skin.image = {{ {images} }}\n\
         {pre}\n\
         skin.imageset = {{ {sets} }}\n\
         skin.value = {{ {values} }}\n\
         skin.gaugegraph = {{ {{ id = \"gg\" }} }}\n\
         skin.destination = {{ {dsts} }}\n\
         return skin\n",
        images = lua_images()
    )
}

/// A digit strip value of `digit` cells and `more` fields.
fn lua_value(id: &str, digit: u32, more: &str) -> String {
    format!(
        "{{ id = \"{id}\", src = \"s\", x = 0, y = 32, w = 40, h = 8, divx = 10, \
         digit = {digit}, {more} }}"
    )
}

/// A destination of `id` at `y` with `more` fields.
fn lua_dst(id: &str, y: i32, w: i32, more: &str) -> String {
    format!("{{ id = \"{id}\", {more} dst = {{ {{ x = 0, y = {y}, w = {w}, h = 8 }} }} }}")
}

/// The Lua skin of [`json_body`]'s elements, each id read through
/// `main_state` where JSON reads it by id, by name where JSON does; and a
/// `draw` `number(178) ~= 0` at y 600, which JSON cannot write.
fn lua_entry() -> String {
    let values = [
        lua_value("ex", 4, "value = function() return ms.number(71) end"),
        lua_value("ex3", 4, "value = \"score3\""),
        lua_value("bpd", 4, "value = function() return ms.number(178) end"),
        lua_value("lv", 2, "value = function() return ms.number(96) end"),
        lua_value("nr", 4, "ref = 154"),
        lua_value("mn", 3, "value = function() return ms.number(374) end"),
    ]
    .join(", ");
    let sets = "{ id = \"clr\", value = function() return ms.event_index(370) end, \
                images = { \"b0\", \"b1\", \"b5\", \"lamp\", \"lab\", \"bar\" } }, \
                { id = \"lane\", ref = 451, images = { \"b0\", \"b1\" } }";
    let dsts = [
        lua_dst("ex", 100, 4, ""),
        lua_dst("ex3", 150, 4, ""),
        lua_dst("bpd", 200, 4, ""),
        lua_dst("lv", 250, 4, ""),
        lua_dst("nr", 260, 4, ""),
        lua_dst("mn", 270, 4, ""),
        lua_dst(
            "dot",
            300,
            8,
            "op = { function() return ms.option(90) end },",
        ),
        lua_dst(
            "lab",
            400,
            8,
            "draw = function() return ms.option(330) end,",
        ),
        lua_dst("clr", 500, 8, ""),
        lua_dst("lane", 550, 8, ""),
        lua_dst(
            "b1",
            600,
            8,
            "draw = function() return ms.number(178) ~= 0 end,",
        ),
        lua_dst("bar", 700, 8, "timer = 2,"),
        lua_dst("lamp", 750, 8, "timer = 173,"),
        lua_dst("gg", 800, 8, ""),
    ]
    .join(", ");
    lua_skin("", &values, sets, &dsts)
}

/// The Lua-only element's top: [`lua_entry`]'s `draw` at y 600.
const DIFF: f32 = 1_080.0 - 600.0 - 8.0;

/// The views of C14 through the importer: ready, then the best's BP
/// hidden, then the best unavailable, then no result, then ready again.
fn views() -> Vec<ResultView> {
    let shown = ready(1_000, true, best(ResultNumber::Shown(-3)));
    vec![
        shown.clone(),
        ready(1_000, true, best(ResultNumber::Hidden)),
        ready(1_000, false, BestView::Unavailable),
        unavailable(),
        shown,
    ]
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

/// C15: a JSON `ref` or name and a Lua read of the same id draw the same
/// frame under every view state, and each value follows its state: shown,
/// hidden (a number draws nothing, an expression reads `i32::MIN`) and
/// unavailable (nothing drawn under its validity); the chart's fields stay
/// with no result; the lane index is 0; the fade-out timer runs and the
/// IR's never does; the graph is dropped with a note on both paths.
#[test]
fn json_and_lua_draw_the_same_result_frames() {
    let j = folder(&json_body());
    let json = import_result(j.path()).unwrap_or_else(|e| panic!("{e:?}"));
    let l = sheet();
    common::write(l.path(), "s.luaskin", lua_entry().as_bytes());
    let lua = lua_result(l.path(), &ImportFacts::unavailable(ScreenKind::Result)).unwrap();
    assert_eq!(notes(&json.report), notes(&lua.imported.report));
    let graph = NoteKind::Unsupported {
        object: Object::GaugeGraph,
    };
    assert!(notes(&json.report).contains(&(Severity::Dropped, graph)));
    let mut a = Res::new(json.skin);
    let mut b = Res::new(lua.imported.skin);
    let lua_only = |q: &skin_model::Quad| q.rect[1] != DIFF;
    for (i, v) in views().iter().enumerate() {
        a.view(v);
        b.view(v);
        let ours: Vec<_> = b.frame(0).into_iter().filter(lua_only).collect();
        assert_eq!(a.frame(0), ours, "view {i}");
    }
    let mut frame = |v: &ResultView| {
        a.view(v);
        b.view(v);
        (
            [EX, EX3, BPD, LV, NR, MN].map(|t| a.digits(0, t)),
            [300.0, 400.0, 500.0, 550.0].map(|y| a.at(0, top(y, 8.0))),
            !b.at(0, DIFF).is_empty(),
        )
    };
    let [v1, v2, v3, v4, v5] = <[ResultView; 5]>::try_from(views()).unwrap();
    // Ready: EX 1000 by id and name, BP difference −3, level 12, 17 to the
    // next grade, the mean −1.49 ms in whole ms truncated toward 0 (its
    // magnitude 1); cleared, the score updated, NORMAL's clear type (5)
    // and lane index 0; `number(178) ~= 0` holds.
    let s = |d: &str| d.to_owned();
    let ready_frame = (
        [s("1000"), s("1000"), s("3"), s("12"), s("17"), s("1")],
        [vec![DOT], vec![LAB], vec![BAR], vec![B0]],
        true,
    );
    assert_eq!(frame(&v1), ready_frame);
    // Hidden: the number draws nothing; the Lua `draw` reads `i32::MIN`,
    // which is not 0, so it draws.
    assert_eq!(
        frame(&v2),
        (
            [s("1000"), s("1000"), s(""), s("12"), s("17"), s("1")],
            [vec![DOT], vec![LAB], vec![BAR], vec![B0]],
            true,
        )
    );
    // The best unavailable: the BP difference, the update mark and the
    // `draw` under its validity draw nothing; failed, FAILED's clear type.
    assert_eq!(
        frame(&v3),
        (
            [s("1000"), s("1000"), s(""), s("12"), s("17"), s("1")],
            [vec![], vec![], vec![B1], vec![B0]],
            false,
        )
    );
    // No result: the chart's level alone, and the constant lane index.
    assert_eq!(
        frame(&v4),
        (
            [s(""), s(""), s(""), s("12"), s(""), s("")],
            [vec![], vec![], vec![], vec![B0]],
            false,
        )
    );
    assert_eq!(frame(&v5), ready_frame);
    // The fade-out timer (2) runs its element; the IR's (173) never.
    assert!(a.at(0, top(700.0, 8.0)).is_empty());
    a.timers.fadeout = Some(bms_model::TimeUs(0));
    assert_eq!(a.at(0, top(700.0, 8.0)), [BAR]);
    assert!(a.at(0, top(750.0, 8.0)).is_empty());
    assert!(!a.drawn(0).iter().any(|(t, _)| *t == LAMP));
}

/// A result skin of `values`, `sets` and `dsts` (JSON members), imported.
fn essential(values: &str, sets: &str, dsts: &str) -> Result<Report, ImportError> {
    let d = folder(&format!(
        r#""imageset": [{sets}], "value": [{values}], "destination": [{dsts}]"#
    ));
    import_result(d.path()).map(|i| i.report)
}

fn value(id: &str, field: &str) -> String {
    format!(r#"{{"id": "{id}", {DIGITS}, "digit": 4, {field}}}"#)
}

fn dst(id: &str, more: &str) -> String {
    format!(r#"{{"id": "{id}", {more} "dst": [{{"x": 0, "y": 0, "w": 8, "h": 8}}]}}"#)
}

#[track_caller]
fn refused(r: Result<Report, ImportError>) -> (EssentialPart, Option<NoteKind>) {
    match r {
        Err(ImportError::Essential { part, why, .. }) => (part, why),
        other => panic!("{other:?}"),
    }
}

/// C16: a result skin needs a retained EX number (71 or 171) and a status
/// element (option 90 or 91 in its visibility, or image index or number
/// 370), judged after folding; an essential element that cannot be
/// reproduced refuses, any other is dropped with a note.
#[test]
fn a_result_skin_needs_its_score_and_status() {
    let ex = value("ex", r#""ref": 71"#);
    let ex3 = value("ex3", r#""value": "score3""#);
    let clear = value("clear", r#""ref": 370"#);
    let set = format!(r#"{{"id": "clr", "ref": 370, "images": {CLEAR_SET}}}"#);
    // Each way of showing each part imports.
    for (values, sets, dsts) in [
        (
            ex.clone(),
            "",
            dst("ex", "") + "," + &dst("dot", r#""op": [90],"#),
        ),
        (
            ex3.clone(),
            "",
            dst("ex3", "") + "," + &dst("dot", r#""op": [-91],"#),
        ),
        (
            ex.clone() + "," + &clear,
            "",
            dst("ex", "") + "," + &dst("clear", ""),
        ),
        (ex.clone(), &*set, dst("ex", "") + "," + &dst("clr", "")),
        (
            ex.clone(),
            "",
            dst("ex", "") + "," + &dst("dot", r#""draw": "result_clear","#),
        ),
    ] {
        essential(&values, sets, &dsts).unwrap_or_else(|e| panic!("{dsts}: {e:?}"));
    }
    // Missing, or folded away (160 is always true on a 7-key result).
    let status = dst("dot", r#""op": [90],"#);
    for (values, dsts, part) in [
        (ex.clone(), dst("ex", ""), EssentialPart::ResultStatus),
        (String::new(), status.clone(), EssentialPart::ResultScore),
        (
            ex.clone(),
            dst("ex", r#""op": [-160],"#) + "," + &status,
            EssentialPart::ResultScore,
        ),
        (
            ex.clone(),
            dst("ex", "") + "," + &dst("dot", r#""op": [90, -160],"#),
            EssentialPart::ResultStatus,
        ),
    ] {
        assert_eq!(
            refused(essential(&values, "", &dsts)),
            (part, None),
            "{dsts}"
        );
    }
    // An essential element that cannot be reproduced (blend 9) refuses;
    // another is dropped with a note.
    let blend = r#""blend": 9,"#;
    for (values, dsts, part) in [
        (
            ex.clone(),
            dst("ex", blend) + "," + &status,
            EssentialPart::ResultScore,
        ),
        (
            ex.clone() + "," + &clear,
            dst("ex", "") + "," + &dst("clear", blend),
            EssentialPart::ResultStatus,
        ),
    ] {
        assert_eq!(
            refused(essential(&values, "", &dsts)),
            (part, Some(NoteKind::Blend(9))),
            "{dsts}"
        );
    }
    let bp = value("bp", r#""ref": 76"#);
    let r = essential(
        &(ex + "," + &bp),
        "",
        &(dst("ex", "") + "," + &status + "," + &dst("bp", blend)),
    )
    .unwrap();
    assert!(
        r.notes
            .iter()
            .any(|n| n.severity == Severity::Dropped && n.kind == NoteKind::Blend(9))
    );
}

/// C16: an image group (`len` above 1) by image index 370 is the clear
/// status, the only one beside an EX number, JSON and Lua alike (the Lua as
/// the source's bundled result skin writes it, a numeric id in
/// `skin.image`).
#[test]
fn a_grouped_clear_image_is_the_status() {
    let group = r#"{"id": "grp", "src": "s", "x": 0, "y": 64, "w": 16, "h": 8, "divx": 2,
                    "len": 2, "ref": 370}"#;
    let d = sheet();
    let body = format!(
        r#"{{"type": 7, "w": 1920, "h": 1080,
            "source": [{{"id": "s", "path": "s.png"}}],
            "font": [{{"id": "f", "path": "f.ttf"}}],
            "image": [{group}],
            "value": [{ex}],
            "destination": [{dex}, {dgrp}]}}"#,
        ex = value("ex", r#""ref": 71"#),
        dex = dst("ex", ""),
        dgrp = dst("grp", ""),
    );
    common::write(d.path(), "s.json", body.as_bytes());
    import_result(d.path()).unwrap_or_else(|e| panic!("{e:?}"));
    let pre = "skin.image[#skin.image + 1] = { id = 100, src = \"s\", x = 0, y = 64, \
               w = 16, h = 8, divx = 2, len = 2, ref = 370 }";
    let ex = lua_value("ex", 4, "ref = 71");
    let dsts =
        lua_dst("ex", 100, 4, "") + ", { id = 100, dst = { { x = 0, y = 300, w = 8, h = 8 } } }";
    let l = sheet();
    common::write(
        l.path(),
        "s.luaskin",
        lua_skin(pre, &ex, "", &dsts).as_bytes(),
    );
    lua_result(l.path(), &ImportFacts::unavailable(ScreenKind::Result))
        .unwrap_or_else(|e| panic!("{e:?}"));
}

/// D7: a negated result option holds only while its value is available,
/// its validity joined outside the negation, JSON (`op` -90, -330, -300)
/// and Lua (`not ms.option(..)` in a `draw` callback) alike: on a view
/// without the result, or without the best, nothing under it draws.
#[test]
fn a_negated_result_option_needs_its_value() {
    let ex = value("ex", r#""ref": 71"#);
    let at = |id: &str, y: i32, more: &str| {
        format!(r#"{{"id": "{id}", {more} "dst": [{{"x": 0, "y": {y}, "w": 8, "h": 8}}]}}"#)
    };
    let dsts = [
        dst("ex", ""),
        at("dot", 300, r#""op": [90],"#),
        at("lab", 400, r#""op": [-90],"#),
        at("bar", 500, r#""op": [-330],"#),
        at("b5", 600, r#""op": [-300],"#),
    ]
    .join(",");
    let j = folder(&format!(r#""value": [{ex}], "destination": [{dsts}]"#));
    let json = import_result(j.path()).unwrap_or_else(|e| panic!("{e:?}"));
    let draw = |id: &str, y: i32, cond: &str| {
        lua_dst(id, y, 8, &format!("draw = function() return {cond} end,"))
    };
    let lua_dsts = [
        lua_dst("ex", 100, 4, ""),
        draw("dot", 300, "ms.option(90)"),
        draw("lab", 400, "not ms.option(90)"),
        draw("bar", 500, "not ms.option(330)"),
        draw("b5", 600, "not ms.option(300)"),
    ]
    .join(", ");
    let l = sheet();
    common::write(
        l.path(),
        "s.luaskin",
        lua_skin("", &lua_value("ex", 4, "ref = 71"), "", &lua_dsts).as_bytes(),
    );
    let lua = lua_result(l.path(), &ImportFacts::unavailable(ScreenKind::Result))
        .unwrap_or_else(|e| panic!("{e:?}"));
    for skin in [json.skin, lua.imported.skin] {
        let mut r = Res::new(skin);
        let mut shown = |v: &ResultView| {
            r.view(v);
            [300.0, 400.0, 500.0, 600.0].map(|y| !r.at(0, top(y, 8.0)).is_empty())
        };
        // Cleared, the score updated, AA (not AAA): 90 and !300 hold.
        let cleared = ready(1_000, true, best(ResultNumber::Shown(-3)));
        assert_eq!(shown(&cleared), [true, false, false, true]);
        // Failed with no best: !90 and !300; !330 needs the best.
        let failed = ready(1_000, false, BestView::Unavailable);
        assert_eq!(shown(&failed), [false, true, false, true]);
        // No result: nothing under a result option draws.
        assert_eq!(shown(&unavailable()), [false; 4]);
    }
}

/// C16: only a number shows the EX score; an image set choosing by 71 or
/// 171, by `value` or a Lua callback, beside a status and no EX number, is
/// refused for the missing score.
#[test]
fn an_image_set_is_not_the_score() {
    let status = dst("dot", r#""op": [90],"#);
    for set in [
        r#"{"id": "s", "value": 71, "images": ["b0", "b1"]}"#,
        r#"{"id": "s", "value": 171, "images": ["b0", "b1"]}"#,
    ] {
        assert_eq!(
            refused(essential("", set, &(dst("s", "") + "," + &status))),
            (EssentialPart::ResultScore, None),
            "{set}"
        );
    }
    let sets = "{ id = \"s\", value = function() return ms.number(71) end, \
                images = { \"b0\", \"b1\" } }";
    let dsts = lua_dst("s", 100, 8, "") + ", " + &lua_dst("dot", 300, 8, "op = { 90 },");
    let d = sheet();
    common::write(
        d.path(),
        "s.luaskin",
        lua_skin("", "", sets, &dsts).as_bytes(),
    );
    match lua_outcome(d.path(), &ImportFacts::unavailable(ScreenKind::Result)) {
        Err(ImportError::Essential { part, .. }) => assert_eq!(part, EssentialPart::ResultScore),
        other => panic!("{other:?}"),
    }
}

/// C16 and C15: a set whose `value` overrides `ref` 370 chooses by its
/// value, so it is not the status, in JSON and in Lua alike: with an EX
/// number and no other status, both refuse for the status.
#[test]
fn an_overridden_clear_ref_is_not_the_status() {
    let ex = value("ex", r#""ref": 71"#);
    let set = r#"{"id": "s", "ref": 370, "value": 71, "images": ["b0", "b1"]}"#;
    assert_eq!(
        refused(essential(&ex, set, &(dst("ex", "") + "," + &dst("s", "")))),
        (EssentialPart::ResultStatus, None)
    );
    let ex = lua_value("ex", 4, "ref = 71");
    let sets = "{ id = \"s\", ref = 370, value = function() return ms.number(71) end, \
                images = { \"b0\", \"b1\" } }";
    let dsts = lua_dst("ex", 100, 4, "") + ", " + &lua_dst("s", 300, 8, "");
    let d = sheet();
    common::write(
        d.path(),
        "s.luaskin",
        lua_skin("", &ex, sets, &dsts).as_bytes(),
    );
    match lua_outcome(d.path(), &ImportFacts::unavailable(ScreenKind::Result)) {
        Err(ImportError::Essential { part, .. }) => assert_eq!(part, EssentialPart::ResultStatus),
        other => panic!("{other:?}"),
    }
}

/// C16: an element that only reads option 90 in its visibility is status
/// when retained, but dropped with a note when it cannot be reproduced
/// (blend 9): the import refuses only when no status remains. An element
/// showing 370 that cannot be reproduced refuses.
#[test]
fn a_status_condition_alone_drops_when_not_reproduced() {
    let ex = value("ex", r#""ref": 71"#);
    let blended = dst("lab", r#""op": [90], "blend": 9,"#);
    let status = dst("dot", r#""op": [91],"#);
    let r = essential(&ex, "", &(dst("ex", "") + "," + &status + "," + &blended))
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert!(
        r.notes
            .iter()
            .any(|n| n.severity == Severity::Dropped && n.kind == NoteKind::Blend(9))
    );
    assert_eq!(
        refused(essential(&ex, "", &(dst("ex", "") + "," + &blended))),
        (EssentialPart::ResultStatus, None)
    );
    let set = format!(r#"{{"id": "clr", "ref": 370, "images": {CLEAR_SET}}}"#);
    assert_eq!(
        refused(essential(
            &ex,
            &set,
            &(dst("ex", "") + "," + &status + "," + &dst("clr", r#""blend": 9,"#))
        )),
        (EssentialPart::ResultStatus, Some(NoteKind::Blend(9)))
    );
}

/// C16 on the Lua path: an EX score read by a callback and a status read
/// by a `draw` callback are the parts; without the score it refuses.
#[test]
fn a_lua_result_skin_needs_its_score_and_status() {
    let ex = lua_value("ex", 4, "value = function() return ms.number(71) end");
    let status = lua_dst("dot", 300, 8, "draw = function() return ms.option(91) end,");
    let d = sheet();
    let facts = ImportFacts::unavailable(ScreenKind::Result);
    common::write(
        d.path(),
        "s.luaskin",
        lua_skin("", &ex, "", &(lua_dst("ex", 100, 4, "") + ", " + &status)).as_bytes(),
    );
    lua_result(d.path(), &facts).unwrap();
    common::write(
        d.path(),
        "s.luaskin",
        lua_skin("", &ex, "", &status).as_bytes(),
    );
    match lua_outcome(d.path(), &facts) {
        Err(ImportError::Essential { part, .. }) => assert_eq!(part, EssentialPart::ResultScore),
        other => panic!("{other:?}"),
    }
}

/// The Lua skin of C17: it reads options 50 and 51, number 154 and number
/// 178 while it loads, placing its status image at x 10 when offline, then
/// 100 further when the BP difference is 0 and 200 further otherwise.
fn load_time() -> String {
    let pre = "local x = 0\n\
               if ms.option(50) and not ms.option(51) then x = 10 end\n\
               local nr = ms.number(154)\n\
               if ms.number(178) == 0 then x = x + 100 else x = x + 200 end";
    let ex = lua_value("ex", 4, "ref = 71");
    let dsts = lua_dst("ex", 100, 4, "")
        + ", { id = \"dot\", op = { 90 }, dst = { { x = x, y = 300, w = 8, h = 8 } } }";
    lua_skin(pre, &ex, "", &dsts)
}

/// Where the status image is drawn for `view`: its left edge.
fn status_x(i: skin_beatoraja::LuaImported, view: &ResultView) -> f32 {
    let mut r = Res::new(i.imported.skin);
    r.view(view);
    let quads = r.frame(0);
    let q: Vec<_> = quads
        .iter()
        .filter(|q| q.rect[1] == top(300.0, 8.0))
        .collect();
    assert_eq!(q.len(), 1, "{q:?}");
    q[0].rect[0]
}

/// D12: the play level (96) a result skin reads while it loads is the
/// one its frames read, the view's chart level, on a view with a result
/// and on one without.
#[test]
fn the_load_time_level_is_the_frames() {
    let pre = "local lv = ms.number(96)";
    let values = lua_value("ex", 4, "ref = 71")
        + ", "
        + &lua_value("lv", 2, "value = function() return ms.number(96) end");
    let dsts = lua_dst("ex", 100, 4, "")
        + ", "
        + &lua_dst("lv", 250, 4, "")
        + ", { id = \"dot\", op = { 90 }, dst = { { x = lv, y = 300, w = 8, h = 8 } } }";
    let d = sheet();
    common::write(
        d.path(),
        "s.luaskin",
        lua_skin(pre, &values, "", &dsts).as_bytes(),
    );
    let level = FactRead {
        fact: FactId::ResultNumber(96),
        value: FactValue::Int(12),
    };
    let v = ready(1_000, true, best(ResultNumber::Shown(-3)));
    let i = lua_result(d.path(), &facts(Some(&v))).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(i.facts.reads, [level]);
    let mut r = Res::new(i.imported.skin);
    r.view(&v);
    assert_eq!(r.digits(0, LV), "12");
    let q = r.frame(0);
    let dot: Vec<_> = q.iter().filter(|q| q.rect[1] == top(300.0, 8.0)).collect();
    assert_eq!(dot.len(), 1);
    assert_eq!(dot[0].rect[0], 12.0);
    let i = lua_result(d.path(), &facts(Some(&unavailable()))).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(i.facts.reads, [level]);
    let mut r = Res::new(i.imported.skin);
    r.view(&unavailable());
    assert_eq!(r.digits(0, LV), "12");
}

/// §A: the total notes (74) are the chart's, as the source reads them
/// (`IntegerPropertyFactory.java:562-576`): drawn on a view without a
/// result too, JSON and Lua, and readable while a Lua skin loads there.
#[test]
fn the_note_count_is_the_charts() {
    let j = folder(&format!(
        r#""value": [{ex}, {n}],
           "destination": [{dex}, {{"id": "n", "dst": [{{"x": 0, "y": 260, "w": 4, "h": 8}}]}},
                           {dot}]"#,
        ex = value("ex", r#""ref": 71"#),
        n = value("n", r#""ref": 74"#),
        dex = dst("ex", ""),
        dot = dst("dot", r#""op": [90],"#),
    ));
    let json = import_result(j.path()).unwrap_or_else(|e| panic!("{e:?}"));
    let pre = "local n = ms.number(74)";
    let values = lua_value("ex", 4, "ref = 71") + ", " + &lua_value("n", 4, "ref = 74");
    let dsts = lua_dst("ex", 100, 4, "")
        + ", "
        + &lua_dst("n", 260, 4, "")
        + ", { id = \"dot\", op = { 90 }, dst = { { x = n // 100, y = 300, w = 8, h = 8 } } }";
    let d = sheet();
    common::write(
        d.path(),
        "s.luaskin",
        lua_skin(pre, &values, "", &dsts).as_bytes(),
    );
    let none = unavailable();
    let lua = lua_result(d.path(), &facts(Some(&none))).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        lua.facts.reads,
        [FactRead {
            fact: FactId::ResultNumber(74),
            value: FactValue::Int(600),
        }]
    );
    let ready = ready(1_000, true, best(ResultNumber::Shown(-3)));
    for skin in [json.skin, lua.imported.skin] {
        let mut r = Res::new(skin);
        for v in [&ready, &none] {
            r.view(v);
            assert_eq!(r.digits(0, NR), "600");
        }
    }
    let mut r = Res::new(
        lua_result(d.path(), &facts(Some(&ready)))
            .unwrap()
            .imported
            .skin,
    );
    r.view(&ready);
    let q = r.frame(0);
    let dot: Vec<_> = q.iter().filter(|q| q.rect[1] == top(300.0, 8.0)).collect();
    assert_eq!(dot.len(), 1);
    assert_eq!(dot[0].rect[0], 6.0);
}

/// C17: while a skin loads, options 50 and 51 are the offline constants,
/// and a result value follows its state: shown as itself, hidden as
/// `i32::MIN` (imported, not refused), unavailable refused; each read is
/// recorded with its value, and a base snapshot ends pending on the first.
#[test]
fn a_lua_skin_reads_result_values_while_it_loads() {
    let d = sheet();
    common::write(d.path(), "s.luaskin", load_time().as_bytes());
    let read = |n, v| FactRead {
        fact: FactId::ResultNumber(n),
        value: FactValue::Int(v),
    };
    // A previous BP equal to this play's: 0, the moved position.
    let zero = ready(1_000, true, best(ResultNumber::Shown(0)));
    let i = lua_result(d.path(), &facts(Some(&zero))).unwrap();
    assert_eq!(i.facts.reads, [read(154, 17), read(178, 0)]);
    assert!(i.facts.holds(&facts(Some(&zero))));
    assert_eq!(status_x(i, &zero), 110.0);
    // No previous BP: hidden, read as `i32::MIN`, the other position.
    let first = ready(1_000, true, best(ResultNumber::Hidden));
    let i = lua_result(d.path(), &facts(Some(&first))).unwrap();
    assert_eq!(i.facts.reads, [read(154, 17), read(178, i32::MIN)]);
    assert!(!i.facts.holds(&facts(Some(&zero))));
    assert_eq!(status_x(i, &first), 210.0);
    // The best unavailable: the read of 178 refuses.
    let unknown = ready(1_000, true, BestView::Unavailable);
    match lua_outcome(d.path(), &facts(Some(&unknown))) {
        Err(ImportError::Lua {
            why: LuaWhy::Unavailable { id, .. },
            ..
        }) => assert_eq!(id, 178),
        other => panic!("{other:?}"),
    }
    // No result: 154 refuses first.
    match lua_outcome(d.path(), &facts(Some(&unavailable()))) {
        Err(ImportError::Lua {
            why: LuaWhy::Unavailable { id, .. },
            ..
        }) => assert_eq!(id, 154),
        other => panic!("{other:?}"),
    }
    // The base snapshot has the result pending: the pass ends there.
    let base = ImportFacts::base(ScreenKind::Result, Fact::Known(false));
    match lua_outcome(d.path(), &base) {
        Ok(LuaOutcome::Pending(p)) => assert_eq!(p.pending, FactId::ResultNumber(154)),
        other => panic!("{other:?}"),
    }
}

/// A result skin with its essential parts and the top-level fields `top`,
/// written as JSON (`"name": value, ...`, ending with a comma) and
/// imported; its timing and report.
fn json_timing(top: &str) -> (ResultTiming, Report) {
    let values = value("ex", r#""ref": 71"#);
    let dsts = dst("ex", "") + "," + &dst("dot", r#""op": [90],"#);
    let d = folder(&format!(
        r#"{top} "value": [{values}], "destination": [{dsts}]"#
    ));
    let i = import_result(d.path()).unwrap_or_else(|e| panic!("{top}: {e:?}"));
    (i.skin.result_timing(), i.report)
}

/// [`json_timing`] as a Lua skin, `top` its statements setting the
/// skin table's fields.
fn lua_timing(top: &str) -> (ResultTiming, Report) {
    let d = sheet();
    let values = lua_value("ex", 4, "ref = 71");
    let dsts = lua_dst("ex", 100, 4, "") + ", " + &lua_dst("dot", 300, 8, "op = { 90 },");
    common::write(
        d.path(),
        "s.luaskin",
        lua_skin(top, &values, "", &dsts).as_bytes(),
    );
    let i = lua_result(d.path(), &ImportFacts::unavailable(ScreenKind::Result))
        .unwrap_or_else(|e| panic!("{top}: {e:?}"));
    (i.imported.skin.result_timing(), i.imported.report)
}

/// The outro clamp notes of `r`.
fn clamps(r: &Report) -> Vec<(Severity, NoteKind)> {
    r.notes
        .iter()
        .filter(|n| matches!(n.kind, NoteKind::OutroClamped { .. }))
        .map(|n| (n.severity, n.kind))
        .collect()
}

/// C1 (S10-09i): a result skin's `fadeout`, `input` and `scene` are read
/// in µs from JSON and Lua alike: absent 0, negative 0, a `scene` of 0 or
/// below none, and a `fadeout` past 10 s clamped to it with a deviation
/// note naming the declaration.
#[test]
fn a_result_skin_declares_its_timing() {
    let ms = |n: i64| TimeUs(n * 1_000);
    let timing = |outro, input, scene| ResultTiming {
        outro,
        input,
        scene,
    };
    let clamped = |declared_ms| vec![(Severity::Deviation, NoteKind::OutroClamped { declared_ms })];
    for (json, lua, want, notes) in [
        ("", "", timing(ms(0), ms(0), None), vec![]),
        (
            r#""fadeout": 500, "input": 1000, "scene": 3600000,"#,
            "skin.fadeout = 500 skin.input = 1000 skin.scene = 3600000",
            timing(ms(500), ms(1_000), Some(ms(3_600_000))),
            vec![],
        ),
        (
            r#""fadeout": -5, "input": -1, "scene": -7,"#,
            "skin.fadeout = -5 skin.input = -1 skin.scene = -7",
            timing(ms(0), ms(0), None),
            vec![],
        ),
        (
            r#""fadeout": 1, "input": 1, "scene": 0,"#,
            "skin.fadeout = 1 skin.input = 1 skin.scene = 0",
            timing(ms(1), ms(1), None),
            vec![],
        ),
        (
            r#""fadeout": 10000,"#,
            "skin.fadeout = 10000",
            timing(MAX_RESULT_OUTRO, ms(0), None),
            vec![],
        ),
        (
            r#""fadeout": 10001,"#,
            "skin.fadeout = 10001",
            timing(MAX_RESULT_OUTRO, ms(0), None),
            clamped(10_001),
        ),
        (
            r#""fadeout": 2147483647, "input": 2147483647, "scene": 2147483647,"#,
            "skin.fadeout = 2147483647 skin.input = 2147483647 skin.scene = 2147483647",
            timing(MAX_RESULT_OUTRO, ms(2_147_483_647), Some(ms(2_147_483_647))),
            clamped(i32::MAX),
        ),
    ] {
        let (j, jr) = json_timing(json);
        assert_eq!(j, want, "json {json}");
        assert_eq!(clamps(&jr), notes, "json {json}");
        let (l, lr) = lua_timing(lua);
        assert_eq!(l, want, "lua {lua}");
        assert_eq!(clamps(&lr), notes, "lua {lua}");
    }
    assert_eq!(MAX_RESULT_OUTRO, ms(10_000));
}

/// S10-09i: number 152, the EX score's difference from the best, draws
/// the best's difference as 172 does (`IntegerPropertyFactory.java:197`,
/// `:232`, one function), and nothing without a best.
#[test]
fn number_152_is_the_best_difference() {
    let d = folder(&format!(
        r#""value": [{}, {}, {}],
           "destination": [{}, {}, {}, {}]"#,
        value("ex", r#""ref": 71"#),
        value("d152", r#""ref": 152"#),
        value("d172", r#""ref": 172"#),
        dst("ex", ""),
        r#"{"id": "d152", "dst": [{"x": 0, "y": 100, "w": 4, "h": 8}]}"#,
        r#"{"id": "d172", "dst": [{"x": 0, "y": 200, "w": 4, "h": 8}]}"#,
        dst("dot", r#""op": [90],"#),
    ));
    let i = import_result(d.path()).unwrap_or_else(|e| panic!("{e:?}"));
    let mut r = Res::new(i.skin);
    r.view(&ready(1_000, true, best(ResultNumber::Shown(-3))));
    assert_eq!(r.digits(1_000, top(100.0, 8.0)), "100");
    assert_eq!(r.digits(1_000, top(200.0, 8.0)), "100");
    r.view(&ready(1_000, true, BestView::Unavailable));
    assert_eq!(r.digits(1_000, top(100.0, 8.0)), "");
    assert_eq!(r.digits(1_000, top(200.0, 8.0)), "");
}
