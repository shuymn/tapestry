//! The captured object graph and the bindings it proves (design.md
//! "beatoraja Lua Skins", how closures are found and captured): entries
//! copied only along static access paths, the bare `_ENV` as the chunk's
//! cell, and host module and helper bindings proven by their captured
//! final binding while the candidate is closed. Each test imports through
//! the candidate's two passes.

use super::callback::Captured;
use super::callback_tests::{gauge, image, import, skin};
use super::root_tests::{analysis, configured, interferes, lowers, opened, script};
use super::{LuaWhy, Unsupported};
use crate::error::ImportError;
use crate::import::callback::LOWERED;

/// GT's shape: the host module a local of a function, not of the chunk,
/// and a critical gauge `draw` built there reading timer 48 against the
/// off value.
const LOCAL_TO_A_FUNCTION: (&str, &str) = (
    "local function make()\n\
       local main_state = require(\"main_state\")\n\
       return function() return main_state.timer(48) == main_state.timer_off_value end\n\
     end\n\
     local draw = make()",
    "draw",
);

/// Rm's shape: the host module a global, and a helper that reads it, a
/// local of a function, called by a critical gauge `draw`.
const GLOBAL_AND_HELPER: (&str, &str) = (
    "main_state = require(\"main_state\")\n\
     local function main()\n\
       local function remain() return main_state.number(106) - main_state.number(110) end\n\
       return function() return remain() > 0 end\n\
     end\n\
     local draw = main()",
    "draw",
);

/// The global host module read directly by a critical gauge `draw`.
const GLOBAL: (&str, &str) = (
    "main_state = require(\"main_state\")",
    "function() return main_state.gauge_type() == 4 end",
);

/// The sources lowered importing `entry`, which must import.
fn lowered(entry: &str) -> Vec<String> {
    LOWERED.with(|l| l.borrow_mut().clear());
    lowers(import(entry));
    LOWERED.with(|l| l.borrow().clone())
}

#[test]
fn a_module_local_to_a_function_is_proven_by_its_captured_binding() {
    let (body, draw) = LOCAL_TO_A_FUNCTION;
    let sources = lowered(&skin(body, &gauge(draw)));
    assert!(
        sources.iter().any(|s| s.starts_with("(!on(")),
        "{sources:?}"
    );
}

#[test]
fn a_global_module_and_its_helpers_are_proven_by_their_captured_bindings() {
    for (body, draw) in [GLOBAL_AND_HELPER, GLOBAL] {
        let sources = lowered(&skin(body, &gauge(draw)));
        assert_eq!(sources.len(), 1, "{body}: {sources:?}");
    }
}

#[test]
fn a_bare_env_reads_the_chunk_s_globals() {
    // `_ENV.main_state` is the global `main_state`.
    let entry = skin(
        "main_state = require(\"main_state\")",
        &gauge("function() return _ENV.main_state.gauge_type() == 4 end"),
    );
    assert_eq!(lowered(&entry).len(), 1);
}

#[test]
fn a_frame_time_writer_of_the_global_module_is_refused_at_admission() {
    let (body, draw) = GLOBAL;
    let entry = skin(
        body,
        &format!(
            "{}{}",
            image("function() main_state = {} return true end"),
            gauge(draw)
        ),
    );
    match import(&entry) {
        Err(ImportError::Lua { why, .. }) => {
            assert_eq!(why, LuaWhy::Effect(Unsupported::Construct("assignment")));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_binding_the_graph_proves_holds_only_while_the_analysis_is_closed() {
    // Each shape lowers above; with a script beside it, the critical
    // reader refuses at the script.
    for (body, draw) in [LOCAL_TO_A_FUNCTION, GLOBAL_AND_HELPER, GLOBAL] {
        interferes(&opened(body, draw), script("draw"));
    }
}

#[test]
fn a_binding_to_anything_but_the_module_is_refused() {
    // The global replaced by a table, and a global `_ENV` with a
    // metatable, whose reads the raw graph does not see.
    for (body, want) in [
        (
            "main_state = { gauge_type = function() return 4 end }",
            Unsupported::Binding("a host module"),
        ),
        (
            "main_state = require(\"main_state\")\nsetmetatable(_ENV, {})",
            Unsupported::Capture("_ENV's metatable"),
        ),
    ] {
        let entry = skin(body, &gauge(GLOBAL.1));
        match import(&entry) {
            Err(ImportError::Lua { why, .. }) => assert_eq!(why, LuaWhy::Effect(want), "{body}"),
            other => panic!("{body}: {other:?}"),
        }
    }
}

#[test]
fn only_static_path_entries_are_copied() {
    // An option table of 100 000 entries, one read by a constant key:
    // that entry alone is copied, and the critical reader folds.
    let entry = skin(
        "for i = 1, 100000 do skin_config.option[\"k\" .. i] = i end",
        &gauge("function() return skin_config.option[\"k5\"] == 5 end"),
    );
    assert_eq!(lowered(&entry), ["true"]);
    let def = configured(&entry);
    let copied: usize = analysis(&def)
        .tables
        .iter()
        .map(std::collections::HashMap::len)
        .sum();
    // `skin_config` in `_ENV`, its `option`, and `k5`.
    assert_eq!(copied, 3);
}

#[test]
fn a_dynamic_key_ends_the_path() {
    // `t[k].x` copies nothing of `t`: not `t.x`, which the key skips.
    let entry = skin(
        "local t = { a = { x = 1 }, x = 2 }\nlocal k = \"a\"",
        &image("function() return t[k].x == 1 end"),
    );
    let def = configured(&entry);
    let an = analysis(&def);
    assert!(an.tables.iter().all(std::collections::HashMap::is_empty));
    assert_eq!(an.names.len(), 1, "only `k`'s value");
}

/// A skin whose configured pass returns a table with 523 distinct keys of
/// 64 005 bytes, which the conversion keeps (33 474 615 bytes), and makes
/// `count` closures, each capturing its own distinct string of 64 005
/// bytes.
fn strings(count: usize) -> String {
    skin(
        &format!(
            "local p = \"x\"\n\
             while #p < 64000 do p = p .. p end\n\
             p = string.sub(p, 1, 64000)\n\
             skin.extra = {{}}\n\
             for i = 1, 523 do skin.extra[p .. string.format(\"%05d\", i)] = 1 end\n\
             skin.customEvents = {{}}\n\
             for i = 1, {count} do\n\
               local own = p .. string.format(\"%05d\", 1000 + i)\n\
               skin.customEvents[i] = {{ action = function() local mine = own return true end }}\n\
             end"
        ),
        "",
    )
}

/// The configured pass of `entry`, or its refusal.
fn configure(entry: &str) -> Result<crate::resolve::Definition, ImportError> {
    let d = super::callback_tests::folder(entry);
    let cancel = super::Cancel::new();
    let h = super::read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let facts = super::facts::ImportFacts::unavailable(skin_model::ScreenKind::Play);
    let config = crate::import::Config::defaults(h.header());
    match h.configured(&config, &facts, &cancel).0 {
        Ok(def) => Ok(def),
        Err(super::ledger::Stop::Fault(e)) => Err(e),
        Err(super::ledger::Stop::Pending(p)) => panic!("{p:?}"),
    }
}

#[test]
fn captured_strings_are_charged_against_the_retained_bytes() {
    // Beside the kept keys, one captured string fits under the 32 MiB
    // (33 554 432 bytes) of retained bytes; a second does not, refused
    // before it is kept.
    let def = configure(&strings(1)).unwrap();
    assert_eq!(analysis(&def).names.len(), 1);
    match configure(&strings(2)) {
        Err(ImportError::Input {
            limit: crate::error::InputLimit::DecodedBytes,
            ..
        }) => {}
        other => panic!("{:?}", other.map(|_| ())),
    }
}

#[test]
fn captures_keep_floats_strings_tables_and_library_functions() {
    let entry = skin(
        "local f, s, floor = 1.5, \"str\", math.floor\n\
         local t = setmetatable({}, {})\n\
         math.floor = function(x) return x end\n\
         local own = math.floor",
        &image("function() local a, b, c, d, e = f, s, floor, t, own return true end"),
    );
    let def = configured(&entry);
    let an = analysis(&def);
    let floor = super::sandbox::LIBS
        .iter()
        .position(|&n| n == "math.floor")
        .and_then(|i| u16::try_from(i).ok())
        .unwrap();
    let str_id = an.names.get(&b"str"[..]).copied().unwrap();
    let c = &an.captured;
    assert!(c.contains(&Captured::Float(1.5_f64.to_bits())), "{c:?}");
    assert!(c.contains(&Captured::Str(str_id)), "{c:?}");
    assert!(c.contains(&Captured::Lib(floor)), "{c:?}");
    assert!(
        c.iter()
            .any(|x| matches!(x, Captured::Table { meta: true, .. })),
        "{c:?}"
    );
    // The skin's own replacement is a closure, never the library's.
    assert_eq!(
        c.iter().filter(|x| matches!(x, Captured::Lib(_))).count(),
        1
    );
    assert!(c.iter().any(|x| matches!(x, Captured::Closure(_))), "{c:?}");
}
