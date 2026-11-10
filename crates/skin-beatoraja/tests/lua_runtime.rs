//! A selection or result skin's runtime through the production path: the
//! header pass, the configured pass keeping its VM, the import routing
//! each callback no expression represents to it, then frames run on the
//! kept VM over the screen's fields (docs/stage3.md S7-07).

mod common;

use skin_beatoraja::lua::Disposition;
#[cfg(feature = "dev-tools")]
use skin_beatoraja::lua::runtime::{CallStatus, RuntimeNote, Stopped};
use skin_beatoraja::lua::runtime::{Pool, ScreenRuntime};
use skin_beatoraja::{Cancel, Config, ImportFacts, LuaImported, LuaOutcome, read_lua_header};
use skin_model::ScreenKind;
#[cfg(feature = "dev-tools")]
use skin_model::{RESULT, SELECTION};

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
    import_in(text).1
}

/// [`import`], with the skin's folder, which its sheet is read from.
fn import_in(text: &str) -> (tempfile::TempDir, LuaImported) {
    import_with(text, &[])
}

/// [`import_in`], with `files` (name and text) beside the skin.
fn import_with(text: &str, files: &[(&str, &str)]) -> (tempfile::TempDir, LuaImported) {
    let d = common::selection::folder("");
    for (name, body) in files {
        common::write(d.path(), name, body.as_bytes());
    }
    common::write(d.path(), "s.luaskin", text.as_bytes());
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let facts = ImportFacts::unavailable(ScreenKind::Selection);
    match h
        .import_runtime(&Config::defaults(h.header()), &facts, &cancel)
        .unwrap()
    {
        LuaOutcome::Imported(i) => (d, i),
        LuaOutcome::Pending(p) => panic!("{p:?}"),
    }
}

/// A selection state's fields for `skin`'s outputs, with `set` named
/// fields given.
#[cfg(feature = "dev-tools")]
fn fields(outputs: usize, set: &[(&str, i32)]) -> Vec<i32> {
    let mut f = vec![0; SELECTION.fields.len() + 2 * outputs];
    for (name, v) in set {
        let (slot, _) = SELECTION.field(name).unwrap();
        f[usize::from(slot)] = *v;
    }
    f
}

/// Each output's value, after the schema's fields.
#[cfg(feature = "dev-tools")]
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

/// Gives this thread's runtime frames a time budget no frame reaches
/// (dev-tools): a test of something else than the time cap, whose outcome
/// a loaded machine's clock must not decide. The tests that run frames
/// therefore need dev-tools; the time cap itself is driven by
/// [`vm_hooks::set_frame_time`](skin_beatoraja::lua::vm_hooks::set_frame_time)
/// and [`vm_hooks::expire_frame_at`](skin_beatoraja::lua::vm_hooks::expire_frame_at).
#[cfg(feature = "dev-tools")]
fn untimed() {
    skin_beatoraja::lua::vm_hooks::set_frame_time(Some(std::time::Duration::from_secs(3_600)));
}

/// C1, C4, C8, C12: callbacks no expression represents run on the kept
/// VM; their reads answer from the screen's fields through the profile,
/// an omitted id with the source's value in the feature's absence; a
/// loader reached through a captured alias or a table field raises the
/// fault marker; an ordinary error gives the source's 0; results convert
/// as the source's `toint` and `toboolean`.
#[cfg(feature = "dev-tools")]
#[test]
fn callbacks_run_on_the_kept_vm() {
    untimed();
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
    // One bridge per profile-known id read (77), kept across frames.
    assert_eq!(runtime(&mut i).counters().bridges, 1);
}

/// C3's gate: a draw whose `op` before it is false is not called; every
/// slot is still written that frame.
#[cfg(feature = "dev-tools")]
#[test]
fn a_false_gate_skips_the_call() {
    untimed();
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
// Covers: T09
#[cfg(feature = "dev-tools")]
#[test]
fn the_budget_stops_the_runtime_after_three_frames() {
    untimed();
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

/// One frame of a runtime whose first callback is `body` and whose second
/// reads a live id, with the frame time `time`: the statuses, and the
/// instructions and host calls the frame spent.
#[cfg(feature = "dev-tools")]
fn over(body: &str, time: std::time::Duration) -> (Vec<CallStatus>, u64, u64) {
    let mut i = import(&entry(
        "",
        &[body, "function() return ms.number(77) / 1 end"],
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    let mut f = fields(n, &[("cursor.chart", 1)]);
    let before = runtime(&mut i).counters();
    skin_beatoraja::lua::vm_hooks::set_frame_time(Some(time));
    runtime(&mut i).frame(&mut f, &[]);
    skin_beatoraja::lua::vm_hooks::set_frame_time(None);
    assert_eq!(outputs(&f), [0, 0]);
    let after = runtime(&mut i).counters();
    (
        runtime(&mut i).status().to_vec(),
        after.instructions - before.instructions,
        after.host_calls - before.host_calls,
    )
}

/// C8, §3.5: each of the frame's caps stops a call on its own, the frame's
/// later calls given their constants: the instructions with time to
/// spare, the host calls within the instructions and time, the time with
/// instructions to spare.
// Covers: T09
#[cfg(feature = "dev-tools")]
#[test]
fn each_frame_cap_stops_a_call_alone() {
    use skin_beatoraja::lua::limits::{INSTRUCTION_STEP, RUNTIME_HOST_CALLS, RUNTIME_INSTRUCTIONS};
    let budget = [CallStatus::Budget, CallStatus::Budget];
    let long = std::time::Duration::from_secs(10);
    let (st, ins, _) = over("function() while true do end end", long);
    assert_eq!(st, budget);
    assert!(
        (RUNTIME_INSTRUCTIONS - u64::from(INSTRUCTION_STEP)..=RUNTIME_INSTRUCTIONS).contains(&ins),
        "{ins}"
    );
    let (st, ins, calls) = over(
        "function() for _ = 1, 3000 do ms.number(77) end return 1 end",
        long,
    );
    assert_eq!(st, budget);
    assert_eq!(calls, u64::from(RUNTIME_HOST_CALLS));
    assert!(ins < RUNTIME_INSTRUCTIONS / 2, "{ins}");
    let (st, ins, _) = over(
        "function() while true do end end",
        std::time::Duration::ZERO,
    );
    assert_eq!(st, budget);
    assert!(ins < RUNTIME_INSTRUCTIONS / 2, "{ins}");
}

/// C8: a panic while a host read holds the context's lock poisons it; the
/// runtime stops through the panic, and the context is read through the
/// poisoned lock from then on, with no second panic: frames, counters,
/// notes and the drop.
#[cfg(feature = "dev-tools")]
#[test]
fn a_poisoned_context_is_read_through() {
    untimed();
    let mut i = import(&entry(
        "",
        &["function() pcall(function() return ms.number(77) end) return 3 end"],
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    let mut f = fields(n, &[("cursor.chart", 1)]);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(runtime(&mut i).counters().bridges, 1);
    skin_beatoraja::lua::vm_hooks::panic_holding_context();
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(runtime(&mut i).stopped(), Some(Stopped::Panic));
    for _ in 0..2 {
        let mut f = fields(n, &[("cursor.chart", 1)]);
        runtime(&mut i).frame(&mut f, &[]);
        assert_eq!(outputs(&f), [0]);
    }
    assert_eq!(runtime(&mut i).counters().bridges, 1);
    let (notes, _) = runtime(&mut i).notes();
    assert!(notes.contains(&RuntimeNote::Stopped(Stopped::Panic)));
    drop(i);
}

/// The runtime's sampled high-water is the kept VM's own (dev-tools,
/// the S7-07d gate): a large heap the import's passes left and freed is
/// not reported as the runtime's, and a runtime frame holding more than
/// the import's high-water is.
#[cfg(feature = "dev-tools")]
#[test]
fn the_runtime_high_water_is_the_kept_vms_own() {
    untimed();
    let mut i = import(&entry(
        "local calls = 0 local big = {} for k = 1, 100000 do big[k] = k end big = nil",
        &["function() calls = calls + 1 return 1 end"],
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    let mut f = fields(n, &[("cursor.chart", 1)]);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(
        runtime(&mut i).runtime_peak(),
        None,
        "the import's peak is not the VM's"
    );
    let mut i = import(&entry(
        "",
        &[
            "function() local s = \"x\" for _ = 1, 21 do s = s .. s end \
           for _ = 1, 300 do end return #s > 0 and 1 or 0 end",
        ],
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    let mut f = fields(n, &[("cursor.chart", 1)]);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(runtime(&mut i).stopped(), None);
    let peak = runtime(&mut i).runtime_peak();
    assert!(peak.is_some_and(|p| p >= 2 << 20), "{peak:?}");
}

/// C8: the kept VM's memory cap stops the runtime at once.
// Covers: T09
#[cfg(feature = "dev-tools")]
#[test]
fn the_memory_cap_stops_the_runtime() {
    untimed();
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

/// C8, §4: a configured pass that leaves more than the runtime's memory
/// cap reachable from a routed callback is refused at the import, its VM
/// not kept; one whose large heap nothing reaches keeps its VM.
// Covers: T09
#[test]
fn a_kept_heap_over_the_cap_refuses_the_import() {
    // 64 distinct strings of 512 KiB: 32 MiB, under the import's cap.
    let big = "local s = \"x\" for _ = 1, 19 do s = s .. s end
               local t = {} for k = 1, 64 do t[k] = s .. k end";
    let text = entry(
        big,
        &["function() return #t / 1 end"],
        "function() return true end",
    );
    let d = common::selection::folder("");
    common::write(d.path(), "s.luaskin", text.as_bytes());
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let facts = ImportFacts::unavailable(ScreenKind::Selection);
    let e = h
        .import_runtime(&Config::defaults(h.header()), &facts, &cancel)
        .unwrap_err();
    assert!(
        matches!(
            e,
            skin_beatoraja::ImportError::Lua {
                why: skin_beatoraja::lua::LuaWhy::Limit(skin_beatoraja::lua::LuaLimit::Memory),
                ..
            }
        ),
        "{e:?}"
    );
    // The same heap, dropped before the callback is made, is collected.
    let freed = format!(
        "{big}
t = nil"
    );
    let mut i = import(&entry(
        &freed,
        &["function() return ms.number(77) / 2 end"],
        "function() return true end",
    ));
    assert_eq!(runtime(&mut i).outputs(), 1);
}

/// C14: the notes are capped; further occurrences are counted, and a
/// fresh id each frame keeps no more.
#[cfg(feature = "dev-tools")]
#[test]
fn notes_are_capped() {
    untimed();
    let mut i = import(&entry(
        "local k = 0",
        &["function() k = k + 1 return ms.number(100000 + k) end"],
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    let cap = skin_beatoraja::limits::MAX_NOTES;
    // Each frame reads one fresh id: frames run until ten reads past the
    // cap were counted (bounded).
    let mut frames = 0;
    while runtime(&mut i).notes().1 < 10 && frames < 4 * cap {
        let mut f = fields(n, &[]);
        runtime(&mut i).frame(&mut f, &[]);
        frames += 1;
    }
    let (notes, dropped) = runtime(&mut i).notes();
    assert_eq!(notes.len(), cap);
    assert!(dropped >= 10, "{dropped} after {frames} frames");
    // Unknown ids are not cached: no bridge was kept for them.
    assert_eq!(runtime(&mut i).counters().bridges, 0);
}

/// C14, O6: the runtimes of one screen lifetime share its note book: a
/// callback's error, keyed by its source site (not its index), is kept
/// once whichever import's runtime raises it, and a fresh book (the next lifetime) keeps
/// it again.
#[cfg(feature = "dev-tools")]
#[test]
fn a_shared_book_keeps_a_sites_error_once_per_lifetime() {
    use skin_beatoraja::lua::runtime::NoteBook;
    untimed();
    let text = entry(
        "local k = 0",
        &["function() k = k + 1 error(\"x\") end"],
        "function() return true end",
    );
    let (mut a, mut b) = (import(&text), import(&text));
    let errors = |book: &NoteBook| {
        book.notes()
            .0
            .iter()
            .filter(|n| matches!(n, RuntimeNote::CallbackError { .. }))
            .count()
    };
    let book = NoteBook::new();
    runtime(&mut a).use_notes(&book);
    runtime(&mut b).use_notes(&book);
    for i in [&mut a, &mut b] {
        let n = runtime(i).outputs();
        runtime(i).frame(&mut fields(n, &[]), &[]);
        assert_eq!(runtime(i).status(), [CallStatus::Error]);
    }
    assert_eq!(errors(&book), 1);
    // The same callback index at another line is another site.
    let mut c = import(&entry(
        "local k = 0
local pad = 1",
        &["function() k = k + 1 error(\"x\") end"],
        "function() return true end",
    ));
    runtime(&mut c).use_notes(&book);
    let n = runtime(&mut c).outputs();
    runtime(&mut c).frame(&mut fields(n, &[]), &[]);
    assert_eq!(errors(&book), 2);
    let fresh = NoteBook::new();
    runtime(&mut b).use_notes(&fresh);
    let n = runtime(&mut b).outputs();
    runtime(&mut b).frame(&mut fields(n, &[]), &[]);
    assert_eq!((errors(&fresh), errors(&book)), (1, 2));
}

/// C14: the cap is the book's: once one runtime filled it, another's new
/// note is counted, not kept.
#[cfg(feature = "dev-tools")]
#[test]
fn a_shared_book_caps_every_runtime_together() {
    use skin_beatoraja::lua::runtime::NoteBook;
    untimed();
    let mut a = import(&entry(
        "local k = 0",
        &["function() k = k + 1 return ms.number(100000 + k) end"],
        "function() return true end",
    ));
    let mut b = import(&entry(
        "",
        &["function() return ms.number(99999) / 2 end"],
        "function() return true end",
    ));
    let book = NoteBook::new();
    runtime(&mut a).use_notes(&book);
    runtime(&mut b).use_notes(&book);
    let cap = skin_beatoraja::limits::MAX_NOTES;
    let mut frames = 0;
    while book.notes().0.len() < cap && frames < 2 * cap {
        let n = runtime(&mut a).outputs();
        runtime(&mut a).frame(&mut fields(n, &[]), &[]);
        frames += 1;
    }
    let before = book.notes().1;
    let n = runtime(&mut b).outputs();
    runtime(&mut b).frame(&mut fields(n, &[]), &[]);
    let (notes, dropped) = book.notes();
    assert_eq!(notes.len(), cap);
    assert_eq!(dropped, before + 1);
    assert!(
        !notes
            .iter()
            .any(|n| matches!(n, RuntimeNote::Absent { id: 99999, .. }))
    );
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
#[cfg(feature = "dev-tools")]
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
        untimed();
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
/// it already loaded, a host module or a file module the pass cached,
/// refused before the cached return.
// Covers: T09
#[cfg(feature = "dev-tools")]
#[test]
fn loaders_raise_at_runtime_however_reached() {
    untimed();
    let (_d, mut i) = import_with(
        &entry(
            "local load = require\n\
         local m = require(\"m\")\n\
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
                "function() require(\"m\") return 5 end",
                "function() return m.v + 4 + ms.number(77) * 0 end",
                "function() return 5 + ms.number(77) * 0 end",
            ],
            "function() return true end",
        ),
        &[("m.lua", "return { v = 1 }")],
    );
    let n = runtime(&mut i).outputs();
    let mut f = fields(n, &[("cursor.chart", 1)]);
    runtime(&mut i).frame(&mut f, &[]);
    let status = runtime(&mut i).status().to_vec();
    assert_eq!(outputs(&f), [0, 0, 0, 0, 0, 0, 0, 0, 5, 5], "{status:?}");
    assert!(
        status[..8].iter().all(|s| *s == CallStatus::Error),
        "{status:?}"
    );
    let (notes, _) = runtime(&mut i).notes();
    assert!(notes.contains(&RuntimeNote::ImportOnly), "{notes:?}");
    // The phase stays: the next frame raises again.
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(outputs(&f), [0, 0, 0, 0, 0, 0, 0, 0, 5, 5]);
}

/// The source's object order (docs/stage3.md S7-07): a number's value is
/// read first, and its draws run only when it is not a sentinel
/// (`SkinNumber.java:138-150`).
#[cfg(feature = "dev-tools")]
#[test]
fn a_sentinel_value_skips_its_draws() {
    untimed();
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

/// The source hides a number whose value is a sentinel on every screen
/// (`SkinNumber.java:135-147`): a selection number whose runtime value is
/// `Integer.MIN_VALUE` draws no digit, beside one that draws its value.
#[cfg(feature = "dev-tools")]
#[test]
fn a_sentinel_number_draws_nothing_on_selection() {
    untimed();
    let (_d, mut i) = import_in(&entry(
        "",
        &[
            "function() return ms.number(283) end",
            "function() return ms.number(77) / 1 end",
        ],
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    let mut f = fields(
        n,
        &[
            ("selected.plays", 42),
            ("selected.played", 1),
            ("cursor.chart", 1),
        ],
    );
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(outputs(&f), [i32::MIN, 42]);
    let mut s = common::selection::Sel::new(i.imported.skin);
    s.sel
        .outputs_mut()
        .copy_from_slice(&f[SELECTION.fields.len()..]);
    let digits = s.digits(0);
    assert!(digits.ends_with("42"), "{digits}");
    assert!(!digits.contains("3648"), "{digits}");
    assert_eq!(digits.trim_start_matches('0'), "42", "{digits}");
}

/// `text` with two image sets of one `dot` appended, chosen by `a` and
/// `b`, each drawn by its own draw callback.
#[cfg(feature = "dev-tools")]
fn with_sets(text: &str, a: &str, a_draw: &str, b: &str, b_draw: &str) -> String {
    text.replace(
        "return skin\n",
        &format!(
            "skin.imageset[#skin.imageset + 1] = {{ id = \"sa\", images = {{ \"dot\" }}, value = {a} }}\n\
             skin.imageset[#skin.imageset + 1] = {{ id = \"sb\", images = {{ \"dot\" }}, value = {b} }}\n\
             skin.destination[#skin.destination + 1] = {{ id = \"sa\", draw = {a_draw},\n\
               dst = {{ {{ x = 600, y = 0, w = 8, h = 8 }} }} }}\n\
             skin.destination[#skin.destination + 1] = {{ id = \"sb\", draw = {b_draw},\n\
               dst = {{ {{ x = 700, y = 0, w = 8, h = 8 }} }} }}\n\
             return skin\n"
        ),
    )
}

/// The source checks an image set's selector before its draw conditions
/// (`SkinImage.java:127-132`), whatever backs the selector: a negative
/// lowered selector (a constant) or runtime one (a property hidden with no
/// chart under the cursor) skips the set's runtime draw, so its side
/// effect never reaches a later callback; a selector that is not negative
/// lets it run.
#[cfg(feature = "dev-tools")]
#[test]
fn a_negative_selector_skips_its_runtime_draws() {
    untimed();
    let text = with_sets(
        &entry(
            "local ca, cb = 0, 0",
            &["function() return ca end", "function() return cb end"],
            "function() return true end",
        ),
        "function() return -1 end",
        "function() ca = ca + 1 return true end",
        "function() return ms.number(77) end",
        "function() cb = cb + 1 return true end",
    );
    let mut i = import(&text);
    let n = runtime(&mut i).outputs();
    // The two counters, the first set's draw, the second set's value and
    // draw.
    assert_eq!(n, 5, "{:?}", i.callbacks);
    let set = |chart| {
        fields(
            n,
            &[
                ("selected.plays", 200),
                ("selected.played", chart),
                ("cursor.chart", chart),
            ],
        )
    };
    assert_eq!(
        i.callbacks
            .iter()
            .filter(|d| **d == Disposition::Lowered)
            .count(),
        2,
        "{:?}",
        i.callbacks
    );
    // No chart: both selectors negative.
    for _ in 0..2 {
        let mut f = set(0);
        runtime(&mut i).frame(&mut f, &[]);
        assert_eq!(outputs(&f), [0, 0, 0, i32::MIN, 0]);
        let st = runtime(&mut i).status().to_vec();
        assert_eq!([st[2], st[4]], [CallStatus::Skipped; 2], "{st:?}");
    }
    // A chart with 200 plays: the property-backed selector is 200, its
    // draw runs.
    for _ in 0..2 {
        let mut f = set(1);
        runtime(&mut i).frame(&mut f, &[]);
    }
    let mut f = set(1);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(outputs(&f), [0, 2, 0, 200, 1]);
}

/// The same with a selector from `ref` alone (image index 370, the selected
/// row's clear type, −1 with no played chart under the cursor): its runtime
/// draw runs only when the selector is not negative.
#[cfg(feature = "dev-tools")]
#[test]
fn a_negative_ref_selector_skips_its_runtime_draws() {
    untimed();
    let text = entry(
        "local c = 0",
        &["function() return c end"],
        "function() return true end",
    )
    .replace(
        "return skin\n",
        "skin.imageset[#skin.imageset + 1] = { id = \"sr\", ref = 370, images = { \"dot\" } }\n\
         skin.destination[#skin.destination + 1] = { id = \"sr\",\n\
           draw = function() c = c + 1 return true end,\n\
           dst = { { x = 600, y = 0, w = 8, h = 8 } } }\n\
         return skin\n",
    );
    let mut i = import(&text);
    let n = runtime(&mut i).outputs();
    // The counter and the draw.
    assert_eq!(n, 2, "{:?}", i.callbacks);
    let set = |played| fields(n, &[("selected.played", played), ("cursor.chart", 1)]);
    for _ in 0..2 {
        let mut f = set(0);
        runtime(&mut i).frame(&mut f, &[]);
        assert_eq!(outputs(&f), [0, 0]);
        assert_eq!(runtime(&mut i).status()[1], CallStatus::Skipped);
    }
    // A played chart: the selector is its lamp index, 0, so the draw runs.
    for _ in 0..2 {
        let mut f = set(1);
        runtime(&mut i).frame(&mut f, &[]);
    }
    let mut f = set(1);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(outputs(&f), [2, 1]);
}

/// A grouped image (`len` above 1) selects by its `ref` as an image set
/// does (`SkinImage.java:127-132`): its runtime draw runs only when that
/// selector is not negative.
#[cfg(feature = "dev-tools")]
#[test]
fn a_grouped_image_s_negative_ref_skips_its_runtime_draws() {
    untimed();
    let text = entry(
        "local c = 0",
        &["function() return c end"],
        "function() return true end",
    )
    .replace(
        "return skin\n",
        "skin.image[#skin.image + 1] = { id = \"gr\", src = \"s\", x = 0, y = 0, w = 8, h = 8, divx = 2, len = 2, ref = 370 }\n\
         skin.destination[#skin.destination + 1] = { id = \"gr\",\n\
           draw = function() c = c + 1 return true end,\n\
           dst = { { x = 600, y = 0, w = 8, h = 8 } } }\n\
         return skin\n",
    );
    let mut i = import(&text);
    let n = runtime(&mut i).outputs();
    assert_eq!(n, 2, "{:?}", i.callbacks);
    let set = |played| fields(n, &[("selected.played", played), ("cursor.chart", 1)]);
    for _ in 0..2 {
        let mut f = set(0);
        runtime(&mut i).frame(&mut f, &[]);
        assert_eq!(outputs(&f), [0, 0]);
        assert_eq!(runtime(&mut i).status()[1], CallStatus::Skipped);
    }
    for _ in 0..2 {
        let mut f = set(1);
        runtime(&mut i).frame(&mut f, &[]);
    }
    let mut f = set(1);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(outputs(&f), [2, 1]);
}

/// C4 on a result screen: a read answers through the result profile's
/// bridge over the result state, absent-feature ids with the source's
/// value.
#[cfg(feature = "dev-tools")]
#[test]
fn a_result_skin_runs_its_callbacks() {
    untimed();
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
/// that drops it, after the VM: when the VM is destroyed the permit is
/// still out.
#[cfg(feature = "dev-tools")]
#[test]
fn a_dropped_runtime_returns_its_permit() {
    let pool = Pool::new(1);
    let i = import(&entry(
        "",
        &["function() return ms.number(77) / 2 end"],
        "function() return true end",
    ));
    let mut rt = i.runtime.unwrap();
    rt.attach_permit(pool.take().unwrap()).unwrap();
    assert_eq!(pool.free(), 0);
    let (tx, rx) = std::sync::mpsc::channel();
    let seen = pool.clone();
    rt.on_vm_drop(move || tx.send(seen.free()).unwrap());
    std::thread::spawn(move || drop(rt)).join().unwrap();
    assert_eq!(rx.try_recv(), Ok(0), "the permit was out at the VM's end");
    assert_eq!((pool.free(), pool.releases()), (1, 1));
}

/// A runtime holds one permit: a second is refused and handed back, and
/// the first stays out until the runtime is dropped.
#[test]
fn a_runtime_holds_one_permit() {
    let pool = Pool::new(2);
    let i = import(&entry(
        "",
        &["function() return ms.number(77) / 2 end"],
        "function() return true end",
    ));
    let mut rt = i.runtime.unwrap();
    rt.attach_permit(pool.take().unwrap()).unwrap();
    let second = rt.attach_permit(pool.take().unwrap()).unwrap_err();
    assert_eq!(pool.free(), 0);
    drop(second);
    assert_eq!((pool.free(), pool.releases()), (1, 1));
    drop(rt);
    assert_eq!((pool.free(), pool.releases()), (2, 2));
}

/// C8: a panic in a host function stops the runtime, even under a `pcall`;
/// every slot is its constant from then on and frames keep running.
// Covers: T09
#[cfg(feature = "dev-tools")]
#[test]
fn a_host_panic_stops_the_runtime() {
    untimed();
    let mut i = import(&entry(
        "",
        &[
            "function() pcall(function() return ms.number(77) end) return 3 end",
            "function() return ms.number(77) / 2 end",
        ],
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    let mut f = fields(n, &[("cursor.chart", 1)]);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(outputs(&f), [3, i32::MIN / 2]);
    skin_beatoraja::lua::vm_hooks::panic_next_host_call();
    let mut f = fields(n, &[("cursor.chart", 1)]);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(runtime(&mut i).stopped(), Some(Stopped::Panic));
    assert_eq!(outputs(&f), [0, 0]);
    let (notes, _) = runtime(&mut i).notes();
    assert!(notes.contains(&RuntimeNote::Stopped(Stopped::Panic)));
    let mut f = fields(n, &[("cursor.chart", 1)]);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(outputs(&f), [0, 0]);
    assert!(
        runtime(&mut i)
            .status()
            .iter()
            .all(|s| *s == CallStatus::Stopped)
    );
}

/// C8: a frame whose time budget runs out part way (driven here, as a
/// loaded machine's clock may) gives the call in progress and every later
/// one the budget's constant, the earlier ones keeping their results; the
/// next frame runs every call again.
#[cfg(feature = "dev-tools")]
#[test]
fn a_budget_spent_part_way_gives_the_rest_their_constant() {
    use skin_beatoraja::lua::vm_hooks;
    untimed();
    let mut i = import(&entry(
        "",
        &[
            "function() return 3 + ms.number(77) / 1 * 0 end",
            "function() return 4 + ms.number(77) / 1 * 0 end",
            "function() return 5 + ms.number(77) / 1 * 0 end",
        ],
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    let mut f = fields(n, &[("cursor.chart", 1)]);
    vm_hooks::expire_frame_at(Some(1));
    runtime(&mut i).frame(&mut f, &[]);
    vm_hooks::expire_frame_at(None);
    assert_eq!(
        runtime(&mut i).status(),
        [CallStatus::Ok, CallStatus::Budget, CallStatus::Budget]
    );
    assert_eq!(outputs(&f), [3, 0, 0]);
    let mut f = fields(n, &[("cursor.chart", 1)]);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(outputs(&f), [3, 4, 5]);
}

/// O6: two callbacks defined on one line are two sites: each one's error
/// is kept.
#[cfg(feature = "dev-tools")]
#[test]
fn two_callbacks_on_one_line_are_two_sites() {
    untimed();
    let mut i = import(&entry(
        "local k = 0\nlocal e1, e2 = function() k = k + 1 error(\"a\") end, function() k = k + 1 error(\"b\") end",
        &["e1", "e2"],
        "function() return true end",
    ));
    let n = runtime(&mut i).outputs();
    assert_eq!(n, 2);
    runtime(&mut i).frame(&mut fields(n, &[]), &[]);
    let (notes, _) = runtime(&mut i).notes();
    let errors = notes
        .iter()
        .filter(|n| matches!(n, RuntimeNote::CallbackError { .. }))
        .count();
    assert_eq!(errors, 2, "{notes:?}");
}

/// s7-06f-design.md F6: on a screen's runtime `main_state.offset` reads
/// live state: the callback calling it raises, as every unbridged host read
/// does there, where the import's configured pass answers it.
#[cfg(feature = "dev-tools")]
#[test]
fn the_runtime_refuses_an_offset_read() {
    untimed();
    let mut i = import(&entry(
        "",
        &["function() return ms.offset(71).w + 1 end"],
        "function() return true end",
    ));
    let routed = i
        .callbacks
        .iter()
        .filter(|d| **d == Disposition::Runtime)
        .count();
    assert_eq!(routed, 1, "{:?}", i.callbacks);
    let n = runtime(&mut i).outputs();
    let mut f = fields(n, &[]);
    runtime(&mut i).frame(&mut f, &[]);
    assert_eq!(runtime(&mut i).status()[0], CallStatus::Error);
}
