//! A selection or result skin's runtime through the production path: the
//! header pass, the configured pass keeping its VM, the import routing
//! each callback no expression represents to it, then frames run on the
//! kept VM over the screen's fields (docs/stage3.md S7-07).

mod common;

use skin_beatoraja::lua::Disposition;
use skin_beatoraja::lua::runtime::{CallStatus, Pool, RuntimeNote, ScreenRuntime, Stopped};
use skin_beatoraja::{Cancel, Config, ImportFacts, LuaImported, LuaOutcome, read_lua_header};
use skin_model::{RESULT, SELECTION, ScreenKind};

/// A Lua selection skin over the selection fixtures' sheet and font, its
/// numbers' `value`s `values` and its `dot` image's `draw` `draw`.
fn entry(prelude: &str, values: &[&str], draw: &str) -> String {
    entry_with(prelude, values, "nil", draw)
}

/// [`entry`], the first number's destination drawn by `number_draw`.
fn entry_with(prelude: &str, values: &[&str], number_draw: &str, draw: &str) -> String {
    let numbers: String = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            format!(
                "{{ id = \"n{i}\", src = \"s\", x = 0, y = 32, w = 40, h = 8, divx = 10, digit = 4,\n\
                 value = {v} }},\n"
            )
        })
        .collect();
    let dsts: String = (0..values.len())
        .map(|i| {
            format!(
                "{{ id = \"n{i}\", draw = {}, dst = {{ {{ x = 0, y = {}, w = 4, h = 8 }} }} }},\n",
                if i == 0 { number_draw } else { "nil" },
                100 + 10 * i
            )
        })
        .collect();
    format!(
        "local skin = {{ type = 5, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         {prelude}\n\
         skin.source = {{ {{ id = \"s\", path = \"s.png\" }} }}\n\
         skin.font = {{ {{ id = \"f\", path = \"f.ttf\" }} }}\n\
         skin.image = {{ {{ id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 }} }}\n\
         skin.imageset = {{ {{ id = \"set\", images = {{ \"dot\", \"dot\", \"dot\", \"dot\", \"dot\", \"dot\" }} }} }}\n\
         skin.text = {{ {{ id = \"t\", font = \"f\", size = 10, ref = 10 }} }}\n\
         skin.songlist = {{ id = \"list\", center = 0,\n\
           liston = {{ {{ id = \"set\", dst = {{ {{ x = 0, y = 900, w = 300, h = 40 }} }} }} }},\n\
           listoff = {{ {{ id = \"set\", dst = {{ {{ x = 0, y = 900, w = 300, h = 40 }} }} }} }},\n\
           text = {{ {{ id = \"t\", dst = {{ {{ x = 0, y = 0, w = 100, h = 10 }} }} }} }} }}\n\
         skin.value = {{ {numbers} }}\n\
         skin.destination = {{ {{ id = \"list\", dst = {{ {{ x = 0, y = 0, w = 0, h = 0 }} }} }},\n\
           {dsts}\n\
           {{ id = \"dot\", op = {{ 2 }}, draw = {draw},\n\
             dst = {{ {{ x = 500, y = 0, w = 8, h = 8 }} }} }} }}\n\
         return skin\n"
    )
}

/// The configured pass of `text` on a selection scene, keeping its VM.
fn import(text: &str) -> LuaImported {
    let d = common::selection::folder("");
    common::write(d.path(), "s.luaskin", text.as_bytes());
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let facts = ImportFacts::unavailable(ScreenKind::Selection);
    match h
        .import_runtime(&Config::defaults(h.header()), &facts, &cancel)
        .unwrap()
    {
        LuaOutcome::Imported(i) => i,
        LuaOutcome::Pending(p) => panic!("{p:?}"),
    }
}

/// A selection state's fields for `skin`'s outputs, with `set` named
/// fields given.
fn fields(outputs: usize, set: &[(&str, i32)]) -> Vec<i32> {
    let mut f = vec![0; SELECTION.fields.len() + 2 * outputs];
    for (name, v) in set {
        let (slot, _) = SELECTION.field(name).unwrap();
        f[usize::from(slot)] = *v;
    }
    f
}

/// Each output's value, after the schema's fields.
fn outputs(f: &[i32]) -> Vec<i32> {
    f[SELECTION.fields.len()..]
        .iter()
        .step_by(2)
        .copied()
        .collect()
}

fn runtime(i: &mut LuaImported) -> &mut ScreenRuntime {
    i.runtime.as_mut().expect("a runtime")
}

/// C1, C4, C8, C12: callbacks no expression represents run on the kept
/// VM; their reads answer from the screen's fields through the profile,
/// an omitted id with the source's value in the feature's absence; a
/// loader reached through a captured alias or a table field raises the
/// fault marker; an ordinary error gives the source's 0; results convert
/// as the source's `toint` and `toboolean`.
#[test]
fn callbacks_run_on_the_kept_vm() {
    let mut i = import(&entry(
        "local req = require\nlocal held = { d = dofile }",
        &[
            "function() return ms.number(77) / 2 end",
            "function() return ms.number(410) + 7 end",
            "function() return ms.number(283) end",
            "function() req(\"x\") return 5 end",
            "function() held.d(\"x\") return 5 end",
            "function() error(\"boom\") end",
        ],
        "function() return ms.number(77) / 2 > 1 end",
    ));
    let routed = i
        .callbacks
        .iter()
        .filter(|d| **d == Disposition::Runtime)
        .count();
    assert_eq!(routed, 7, "{:?}", i.callbacks);
    assert_eq!(i.imported.skin.outputs(), 7);
    let n = runtime(&mut i).outputs();
    assert_eq!(n, 7);
    let mut f = fields(
        n,
        &[
            ("selected.plays", 9),
            ("selected.played", 1),
            ("cursor.chart", 1),
        ],
    );
    runtime(&mut i).frame(&mut f, &[]);
    // 9 / 2 = 4.5 converts to 4; 0 + 7; the rival's count with no rival is
    // hidden; the two loaders and the error give 0; the draw is true.
    let st = runtime(&mut i).status().to_vec();
    assert_eq!(outputs(&f), [4, 7, i32::MIN, 0, 0, 0, 1], "{st:?}");
    let status = runtime(&mut i).status().to_vec();
    assert_eq!(
        status,
        [
            CallStatus::Ok,
            CallStatus::Ok,
            CallStatus::Ok,
            CallStatus::Error,
            CallStatus::Error,
            CallStatus::Error,
            CallStatus::Ok
        ]
    );
    let (notes, dropped) = runtime(&mut i).notes();
    assert_eq!(dropped, 0);
    assert!(notes.contains(&RuntimeNote::ImportOnly), "{notes:?}");
    for (api, id, value) in [("number", 410, 0), ("number", 283, i32::MIN)] {
        assert!(
            notes.contains(&RuntimeNote::Absent { api, id, value }),
            "{notes:?}"
        );
    }
    // The runtime keeps running: the next frame answers the new fields.
    let mut f = fields(
        n,
        &[
            ("selected.plays", 1),
            ("selected.played", 1),
            ("cursor.chart", 1),
        ],
    );
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(outputs(&f), [0, 7, i32::MIN, 0, 0, 0, 0]);
}

/// C3's gate: a draw whose `op` before it is false is not called; every
/// slot is still written that frame.
#[test]
fn a_false_gate_skips_the_call() {
    let mut i = import(&entry(
        "local calls = 0",
        &["function() return calls end"],
        "function() calls = calls + 1 return true end",
    ));
    let n = runtime(&mut i).outputs();
    let mut f = fields(n, &[]);
    // A stale output from an earlier frame is overwritten.
    let last = f.len() - 2;
    f[last] = 1;
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(runtime(&mut i).status()[1], CallStatus::Skipped);
    assert_eq!(outputs(&f), [0, 0]);
    let mut f = fields(n, &[("cursor.chart", 1)]);
    runtime(&mut i).frame(&mut f, &[]);
    let mut f = fields(n, &[("cursor.chart", 1)]);
    runtime(&mut i).frame(&mut f, &[]);
    // The draw ran once a frame on the two chart frames; the value read
    // its count before the second.
    assert_eq!(outputs(&f), [1, 1]);
}

/// C8: a callback past the frame's budget gives its constant; after three
/// such frames the runtime stops, every slot its constant from then on,
/// and frames keep running.
#[test]
fn the_budget_stops_the_runtime_after_three_frames() {
    let mut i = import(&entry(
        "",
        &[
            "function() while true do end end",
            "function() return ms.number(77) / 1 end",
        ],
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    for frame in 0..3 {
        let mut f = fields(n, &[("cursor.chart", 1)]);
        runtime(&mut i).frame(&mut f, &[]);
        assert_eq!(outputs(&f), [0, 0], "frame {frame}");
        assert_eq!(
            runtime(&mut i).status(),
            [CallStatus::Budget, CallStatus::Budget]
        );
    }
    assert_eq!(runtime(&mut i).stopped(), Some(Stopped::Budget));
    let mut f = fields(n, &[("cursor.chart", 1)]);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(outputs(&f), [0, 0]);
    assert!(
        runtime(&mut i)
            .status()
            .iter()
            .all(|s| *s == CallStatus::Stopped)
    );
    let (notes, _) = runtime(&mut i).notes();
    assert!(notes.contains(&RuntimeNote::Stopped(Stopped::Budget)));
}

/// C8: the kept VM's memory cap stops the runtime at once.
#[test]
fn the_memory_cap_stops_the_runtime() {
    let mut i = import(&entry(
        "",
        &["function() local s = \"x\" for _ = 1, 40 do s = s .. s end return 1 end"],
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    let mut f = fields(n, &[("cursor.chart", 1)]);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(runtime(&mut i).stopped(), Some(Stopped::Memory));
    assert_eq!(outputs(&f), [0]);
}

/// C14: the notes are capped; further occurrences are counted, and a
/// fresh id each frame keeps no more.
#[test]
fn notes_are_capped() {
    let mut i = import(&entry(
        "local k = 0",
        &["function() k = k + 1 return ms.number(100000 + k) end"],
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    let cap = skin_beatoraja::limits::MAX_NOTES;
    for _ in 0..cap + 10 {
        let mut f = fields(n, &[]);
        runtime(&mut i).frame(&mut f, &[]);
    }
    let (notes, dropped) = runtime(&mut i).notes();
    assert_eq!(notes.len(), cap);
    assert!(dropped >= 10, "{dropped}");
}

/// C2: with no callback for the runtime, or through `import`, no VM is
/// kept.
#[test]
fn no_runtime_without_a_routed_callback() {
    let i = import(&entry(
        "",
        &["function() return ms.number(77) end"],
        "function() return ms.option(2) end",
    ));
    assert!(i.runtime.is_none());
    assert_eq!(i.imported.skin.outputs(), 0);
    let d = common::selection::folder("");
    let text = entry("", &["function() return ms.number(77) / 2 end"], "true");
    common::write(d.path(), "s.luaskin", text.as_bytes());
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let facts = ImportFacts::unavailable(ScreenKind::Selection);
    let LuaOutcome::Imported(i) = h
        .import(&Config::defaults(h.header()), &facts, &cancel)
        .unwrap()
    else {
        panic!()
    };
    assert!(i.runtime.is_none());
    assert!(!i.callbacks.contains(&Disposition::Runtime));
}

/// The runtime moves to the screen's thread.
#[test]
fn the_runtime_is_send() {
    fn send<T: Send>() {}
    send::<ScreenRuntime>();
    let mut i = import(&entry(
        "",
        &["function() return ms.number(77) / 2 end"],
        "function() return true end",
    ));
    let mut rt = i.runtime.take().unwrap();
    let n = rt.outputs();
    let out = std::thread::spawn(move || {
        let mut f = fields(
            n,
            &[
                ("selected.plays", 8),
                ("selected.played", 1),
                ("cursor.chart", 1),
            ],
        );
        rt.frame(&mut f, &[]);
        outputs(&f)
    })
    .join()
    .unwrap();
    assert_eq!(out, [4]);
}

/// C1: at runtime, every loader and import-only capability raises the
/// fault marker however a callback reaches it: the global, a renamed
/// local, an upvalue of a helper, a table field, and `require` of a module
/// it already loaded, refused before the cached return.
#[test]
fn loaders_raise_at_runtime_however_reached() {
    let mut i = import(&entry(
        "local load = require\n\
         local function via(f) return function() return f(\"main_state\") end end\n\
         local up = via(require)\n\
         local held = { r = require, d = dofile }\n\
         local cfg = skin_config",
        &[
            "function() require(\"main_state\") return 5 end",
            "function() load(\"main_state\") return 5 end",
            "function() up() return 5 end",
            "function() held.r(\"main_state\") return 5 end",
            "function() held.d(\"x.lua\") return 5 end",
            "function() dofile(\"x.lua\") return 5 end",
            "function() cfg.get_path(\"s.png\") return 5 end",
            "function() return 5 + ms.number(77) * 0 end",
        ],
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    let mut f = fields(n, &[("cursor.chart", 1)]);
    runtime(&mut i).frame(&mut f, &[]);
    let status = runtime(&mut i).status().to_vec();
    assert_eq!(outputs(&f), [0, 0, 0, 0, 0, 0, 0, 5], "{status:?}");
    assert!(
        status[..7].iter().all(|s| *s == CallStatus::Error),
        "{status:?}"
    );
    let (notes, _) = runtime(&mut i).notes();
    assert!(notes.contains(&RuntimeNote::ImportOnly), "{notes:?}");
    // The phase stays: the next frame raises again.
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(outputs(&f), [0, 0, 0, 0, 0, 0, 0, 5]);
}

/// The source's object order (docs/stage3.md S7-07): a number's value is
/// read first, and its draws run only when it is not a sentinel
/// (`SkinNumber.java:138-150`).
#[test]
fn a_sentinel_value_skips_its_draws() {
    let mut i = import(&entry_with(
        "local calls = 0",
        &[
            "function() return ms.number(77) / 1 end",
            "function() return calls end",
        ],
        "function() calls = calls + 1 return true end",
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    assert_eq!(n, 3);
    // No chart under the cursor: the plays are hidden, the draw not run.
    let mut f = fields(n, &[]);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(runtime(&mut i).status()[1], CallStatus::Skipped);
    assert_eq!(outputs(&f), [i32::MIN, 0, 0]);
    let mut f = fields(
        n,
        &[
            ("selected.plays", 3),
            ("selected.played", 1),
            ("cursor.chart", 1),
        ],
    );
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(outputs(&f), [3, 1, 1]);
}

/// C4 on a result screen: a read answers through the result profile's
/// bridge over the result state, absent-feature ids with the source's
/// value.
#[test]
fn a_result_skin_runs_its_callbacks() {
    let tables = "local ms = require(\"main_state\")\n\
        skin.value[2] = { id = \"r\", src = \"s\", x = 0, y = 32, w = 40, h = 8, divx = 10, \
            digit = 4, value = function() return ms.number(71) / 2 end }\n\
        skin.value[3] = { id = \"q\", src = \"s\", x = 0, y = 32, w = 40, h = 8, divx = 10, \
            digit = 4, value = function() return ms.number(385) + 1000 end }";
    let dsts = "{ id = \"r\", dst = { { x = 0, y = 120, w = 4, h = 8 } } },\n\
        { id = \"q\", dst = { { x = 0, y = 140, w = 4, h = 8 } } },";
    let d = common::result::sheet();
    common::write(
        d.path(),
        "s.luaskin",
        common::result::graph_lua(tables, dsts).as_bytes(),
    );
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let facts = common::result::facts(None);
    let LuaOutcome::Imported(mut i) = h
        .import_runtime(&Config::defaults(h.header()), &facts, &cancel)
        .unwrap()
    else {
        panic!()
    };
    let n = runtime(&mut i).outputs();
    assert_eq!(n, 2);
    let mut f = vec![0; RESULT.fields.len() + 2 * n];
    for (name, v) in [("result.valid", 1), ("ex_score", 9)] {
        let (slot, _) = RESULT.field(name).unwrap();
        f[usize::from(slot)] = v;
    }
    runtime(&mut i).frame(&mut f, &[]);
    let out: Vec<i32> = f[RESULT.fields.len()..]
        .iter()
        .step_by(2)
        .copied()
        .collect();
    // 9 / 2; with no ranking data the IR score is `Integer.MIN_VALUE`, and
    // the source's int arithmetic wraps where LuaJ's doubles convert.
    assert_eq!(out, [4, -2_147_482_648], "{:?}", runtime(&mut i).status());
}

/// A runtime's permit returns when the runtime is dropped, on the thread
/// that drops it.
#[test]
fn a_dropped_runtime_returns_its_permit() {
    let pool = Pool::new(1);
    let i = import(&entry(
        "",
        &["function() return ms.number(77) / 2 end"],
        "function() return true end",
    ));
    let rt = i.runtime.unwrap().with_permit(pool.take().unwrap());
    assert_eq!(pool.free(), 0);
    std::thread::spawn(move || drop(rt)).join().unwrap();
    assert_eq!((pool.free(), pool.releases()), (1, 1));
}
