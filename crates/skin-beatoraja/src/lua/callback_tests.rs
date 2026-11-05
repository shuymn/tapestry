//! The callbacks' refusals, bounds and differential oracle, which need
//! the crate's private witnesses: the host calls made while captures are
//! copied ([`super::tests::CAPTURE_CALLS`]), the lowered sources
//! ([`crate::import::callback::LOWERED`]) and a cancellation at an
//! expansion ([`super::tests::at_expansion`]).

use std::cell::Cell;
use std::collections::HashMap;

use skin_model::{PLAY, Scope, State, Ty, compile};

use super::tests::{CAPTURE_CALLS, why};
use super::{Cancel, LuaLimit, LuaWhy, Unsupported, read_lua_header};
use crate::error::ImportError;
use crate::import::Config;
use crate::import::callback::LOWERED;

/// A skin folder whose `s.luaskin` is `entry`, with a 64×64 `a.png`.
pub(super) fn folder(entry: &str) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("s.luaskin"), entry).unwrap();
    let file = std::fs::File::create(d.path().join("a.png")).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), 64, 64);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .unwrap()
        .write_image_data(&[255_u8; 64 * 64 * 4])
        .unwrap();
    d
}

/// A skin whose configured pass runs `body` with `ms` the host module,
/// source `a`, image `i` and gauge nodes `n0`..`n3`, then `dst`, the
/// destinations' list.
pub(super) fn skin(body: &str, dst: &str) -> String {
    format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         {body}\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }},\n\
           {{ id = \"n0\", src = \"a\", x = 0, y = 0, w = 8, h = 8 }}, {{ id = \"n1\", src = \"a\", x = 8, y = 0, w = 8, h = 8 }},\n\
           {{ id = \"n2\", src = \"a\", x = 16, y = 0, w = 8, h = 8 }}, {{ id = \"n3\", src = \"a\", x = 24, y = 0, w = 8, h = 8 }} }}\n\
         skin.gauge = {{ id = \"g\", nodes = {{ \"n0\", \"n1\", \"n2\", \"n3\" }}, parts = 10 }}\n\
         skin.destination = {{ {dst} }}\n\
         return skin\n"
    )
}

/// An image destination drawn under `draw`.
pub(super) fn image(draw: &str) -> String {
    format!("{{ id = \"i\", draw = {draw}, dst = {{ {{ x = 0, y = 0, w = 64, h = 64 }} }} }},")
}

/// A gauge destination drawn under `draw`: critical.
pub(super) fn gauge(draw: &str) -> String {
    format!("{{ id = \"g\", draw = {draw}, dst = {{ {{ x = 100, y = 500, w = 100, h = 10 }} }} }},")
}

fn run(entry: &str, cancel: &Cancel) -> Result<super::LuaImported, ImportError> {
    run_with(entry, &[], cancel)
}

/// [`run`] with `files` beside the entry.
pub(super) fn run_with(
    entry: &str,
    files: &[(&str, &str)],
    cancel: &Cancel,
) -> Result<super::LuaImported, ImportError> {
    let d = folder(entry);
    for (name, text) in files {
        std::fs::write(d.path().join(name), text).unwrap();
    }
    let h = read_lua_header(d.path(), "s.luaskin", cancel)?;
    super::tests::play(&h, &Config::defaults(h.header()), cancel)
}

/// Imports `entry` and checks that copying its captures made no host
/// call, so ran no skin handler.
pub(super) fn import(entry: &str) -> Result<super::LuaImported, ImportError> {
    CAPTURE_CALLS.with(|n| n.set(0));
    let r = run(entry, &Cancel::new());
    assert_eq!(CAPTURE_CALLS.with(Cell::get), 0, "{entry}");
    r
}

/// A test handler: any skin code a capture ran would call it, a host
/// call (`pcall`'s wrapper charges one).
const PROBE: &str = "local function probe() pcall(type, 1) end";

/// The configured pass of `entry`, with `files` beside it, imported: its
/// refusal, if any, and what became of each callable however far it got.
pub(super) fn outcome(
    entry: &str,
    files: &[(&str, &str)],
) -> (Result<(), ImportError>, Vec<super::Disposition>) {
    let d = folder(entry);
    for (name, text) in files {
        std::fs::write(d.path().join(name), text).unwrap();
    }
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let facts = super::facts::ImportFacts::unavailable(skin_model::ScreenKind::Play);
    CAPTURE_CALLS.with(|n| n.set(0));
    let o = h.complete(&Config::defaults(h.header()), &facts, &cancel);
    assert_eq!(CAPTURE_CALLS.with(Cell::get), 0, "{entry}");
    let r = match o.result {
        Ok(_) => Ok(()),
        Err(super::ledger::Stop::Fault(e)) => Err(e),
        Err(super::ledger::Stop::Pending(p)) => panic!("{p:?}"),
    };
    (r, o.callbacks)
}

#[test]
fn refusals_name_their_reason_and_run_no_handler() {
    use super::Disposition::{Lowered, Refused, Replaced};
    let critical = |why| Some(LuaWhy::Critical { field: "draw", why });
    for (body, dst, want, callbacks) in [
        // The module reassigned at construction: the candidate stays
        // closed, and the callback folds through the table it holds, as
        // Lua reads it; written through an alias, fatal.
        (
            "ms = { number = function() return 1 end }",
            gauge("function() return ms.number(110) > 0 end"),
            None,
            vec![Lowered],
        ),
        (
            "local m = ms\npcall(function() m.number = nil end)",
            gauge("function() return ms.number(110) > 0 end"),
            Some(LuaWhy::HostWrite),
            vec![],
        ),
        // An impure helper: its caller writes.
        (
            "local n = 0\nlocal function bump() n = n + 1 return true end",
            gauge("function() return bump() end"),
            critical(Unsupported::Writes),
            vec![Refused],
        ),
        // Integer truthiness folds, the unmapped read it drops still
        // refused.
        (
            "",
            image("function() return ms.number(1) and true end"),
            None,
            vec![Replaced(Unsupported::Unmapped { api: 1, id: 1 })],
        ),
        // Recursion, through a helper bound after its caller: the
        // captured final binding proves it.
        (
            "local f\nlocal function g() return f() end\nf = function() return g() end",
            image("function() return g() end"),
            None,
            vec![Replaced(Unsupported::Recursion)],
        ),
        (
            "local function r() return r() end",
            image("function() return r() end"),
            None,
            vec![Replaced(Unsupported::Recursion)],
        ),
        // `/` in a comparison.
        (
            "",
            image("function() return ms.number(110) / 2 > 0 end"),
            None,
            vec![Replaced(Unsupported::Construct("/"))],
        ),
        // A global nothing set folds to nil.
        ("", image("function() return flag end"), None, vec![Lowered]),
        // An upvalue mutated by the callback.
        (
            "local n = 0",
            image("function() n = n + 1 return true end"),
            None,
            vec![Replaced(Unsupported::Writes)],
        ),
        // A decorative writer beside a critical gauge callback: the
        // writer is replaced, and the module binding the gauge needs
        // fails at the write.
        (
            "",
            format!(
                "{}{}",
                image("function() ms.set_timer(1, 0) return true end"),
                gauge("function() return ms.gauge_type() == 4 end")
            ),
            critical(Unsupported::Interference),
            vec![Replaced(Unsupported::Writes), Refused],
        ),
        (
            "",
            image("function() return ms.set_timer(1, 0) end"),
            None,
            vec![Replaced(Unsupported::Writes)],
        ),
        // A forbidden string operation.
        (
            "",
            image("function() return (\"\").x end"),
            None,
            vec![Replaced(Unsupported::Construct("index"))],
        ),
        // A table comparison.
        (
            "",
            image("function() return {} == {} end"),
            None,
            vec![Replaced(Unsupported::Construct("table"))],
        ),
        // `<close>` and a generic `for`.
        (
            "",
            image("function() local x <close> = nil return true end"),
            None,
            vec![Replaced(Unsupported::Construct("<close>"))],
        ),
        (
            "",
            image("function() for k in pairs({}) do end return true end"),
            None,
            vec![Replaced(Unsupported::Construct("generic for"))],
        ),
        // `_ENV` with a metatable, and `skin_config.option` with one, each
        // read through it opening the candidate. Without the raw entry, it folds
        // to nil; a string entry is unequal to a numeral; a replaced
        // `skin_config` is read as Lua reads it.
        (
            &format!("{PROBE}\nsetmetatable(_ENV, {{ __index = function() probe() end }})"),
            image("function() return skin_config.option[\"x\"] == 1 end"),
            None,
            vec![Replaced(Unsupported::Interference)],
        ),
        (
            &format!(
                "{PROBE}\nsetmetatable(skin_config.option, {{ __index = function() probe() return 1 end }})"
            ),
            image("function() return skin_config.option[\"x\"] == 1 end"),
            None,
            vec![Replaced(Unsupported::Interference)],
        ),
        (
            "",
            image("function() return skin_config.option[\"x\"] == 1 end"),
            None,
            vec![Lowered],
        ),
        (
            "skin_config.option.x = \"1\"",
            image("function() return skin_config.option[\"x\"] == 1 end"),
            None,
            vec![Lowered],
        ),
        (
            "skin_config = { option = { x = 1 } }",
            image("function() return skin_config.option[\"x\"] == 1 end"),
            None,
            vec![Lowered],
        ),
        // A float.
        (
            "",
            image("function() return ms.number(110) > 1.0 end"),
            None,
            vec![Replaced(Unsupported::Construct("float"))],
        ),
    ] {
        let entry = skin(body, &dst);
        let (r, got) = outcome(&entry, &[]);
        let why = match r {
            Ok(()) => None,
            Err(ImportError::Lua { why, .. }) => Some(why),
            Err(e) => panic!("{body} / {dst}: {e:?}"),
        };
        assert_eq!((why, got), (want, callbacks), "{body} / {dst}");
    }
}

/// Chunk A of the capture proof's guard: `cfg` aliases `skin_config` at
/// chunk level, and its helper `w` runs `body` at frame time, on line 3.
pub(super) fn writer_chunk(body: &str) -> String {
    format!(
        "local cfg = skin_config\nlocal function w()\n  {body}\n  return true\nend\nreturn {{ w = w }}\n"
    )
}

/// Chunk B: binds A's export at construction; a decorative `draw` calls
/// it, beside a critical gauge `draw` reading the option `x` (1).
pub(super) fn reader_entry() -> String {
    skin(
        "local w = require(\"a\").w\nskin_config.option.x = 1",
        &format!(
            "{}{}",
            image("function() return w() end"),
            gauge("function() return skin_config.option[\"x\"] == 1 end")
        ),
    )
}

#[test]
fn what_a_callback_cannot_reach_is_allowed() {
    // A load-time edit of the strings' metatable, refused as Lua refuses
    // it, and a load-time setter on an unrelated table: no accepted
    // callback reaches either.
    let entry = skin(
        "pcall(function() getmetatable(\"\").__index = {} end)\n\
         local t = setmetatable({}, { __newindex = function() end })\n\
         t.x = 1\n\
         skin_config.option.x = 2",
        &format!(
            "{}{}",
            image("function() return skin_config.option[\"x\"] == 2 end"),
            gauge("function() return ms.gauge_type() == 4 end")
        ),
    );
    let i = import(&entry).unwrap();
    assert!(
        i.callbacks
            .iter()
            .all(|d| *d == super::Disposition::Lowered)
    );
}

#[test]
fn bounds_are_fatal_even_for_decorative_callbacks() {
    // The parser's depth, before Lua's own compiler, in a module only the
    // configured pass loads.
    let deep = format!("{}1{}", "(".repeat(300), ")".repeat(300));
    let entry = skin("local deep = require(\"deep\")", "");
    let module = format!("return function() return {deep} > 0 end");
    assert_eq!(
        why(run_with(&entry, &[("deep.lua", &module)], &Cancel::new())),
        LuaWhy::Limit(LuaLimit::AstDepth)
    );
    // The parser's nodes: a long table, three nodes an item.
    let items = "1,".repeat(400_000);
    let entry = skin(&format!("local big = {{ {items} }}"), "");
    assert_eq!(why(import(&entry)), LuaWhy::Limit(LuaLimit::AstNodes));
    // Helper expansion: 31 nested `p() or p()`.
    let mut helpers = String::from("local function p0() return ms.number(110) > 0 end\n");
    for k in 1..=31 {
        helpers.push_str(&format!(
            "local function p{k}() return p{0}() or p{0}() end\n",
            k - 1
        ));
    }
    let entry = skin(&helpers, &image("function() return p31() end"));
    assert_eq!(why(import(&entry)), LuaWhy::Limit(LuaLimit::Expansion));
    // A cancellation seen while expanding.
    let entry = skin(&helpers, &image("function() return p3() end"));
    let cancel = Cancel::new();
    let remote = cancel.clone();
    super::tests::at_expansion(move || remote.cancel());
    assert_eq!(why(run(&entry, &cancel)), LuaWhy::Cancelled);
}

#[test]
fn every_walk_over_a_tree_stops_at_the_depth_cap() {
    let fatal = LuaWhy::Limit(LuaLimit::AstDepth);
    // Classification, at admission, of a root nothing consumes: a
    // left-leaning chain the parser reads without recursing.
    let chain = vec!["1"; 100_000].join("+");
    let entry = skin(
        &format!(
            "skin.customEvents = {{ {{ id = 1000, action = function() return {chain} == 0 end }} }}"
        ),
        "",
    );
    assert_eq!(why(import(&entry)), fatal);
    // Evaluation through helpers, each calling the one before.
    let mut helpers = String::from("local function h0() return ms.number(110) > 0 end\n");
    for k in 1..150 {
        helpers.push_str(&format!(
            "local function h{k}() return h{}() and true end\n",
            k - 1
        ));
    }
    let entry = skin(&helpers, &image("function() return h149() end"));
    assert_eq!(why(import(&entry)), fatal);
    // Evaluation binds each local to its node once: a long chain of
    // locals is shallow, and its residual's height past the expressions'
    // nesting follows the role.
    let mut locals = String::from("local a0 = ms.number(110) ");
    for k in 1..150 {
        locals.push_str(&format!("local a{k} = a{} + 1 ", k - 1));
    }
    let entry = skin(
        "",
        &image(&format!("function() {locals} return a149 > 0 end")),
    );
    assert_eq!(
        import(&entry).unwrap().callbacks,
        [super::Disposition::Replaced(super::residual::LIMITS)]
    );
}

/// A skin whose configured pass makes `count` closures of one function
/// whose body is 180 locals of 180-term sums, each capturing its own
/// loop index when `distinct`.
fn many(count: usize, distinct: bool) -> String {
    let sum = vec!["1"; 180].join("+");
    let body: String = (0..180).map(|i| format!("local v{i}={sum}\n")).collect();
    let own = if distinct { "local own = k\n" } else { "" };
    format!(
        "local s = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return s end\n\
         s.customEvents = {{}}\n\
         for i = 1, {count} do local k = i s.customEvents[i] = {{ action = function()\n\
         {own}{body}return true\n\
         end }} end\n\
         return s"
    )
}

#[test]
fn a_function_made_in_a_loop_is_classified_once() {
    // 60 000 closures of one function, nothing captured: its body is
    // walked once.
    let i = import(&many(60_000, false)).unwrap();
    assert_eq!(i.callbacks.len(), 60_000);
    // A few units a callable (its module name hashed, its admission), and
    // the body once: far below 60 000 bodies.
    assert!(
        i.usage.analysis_work < 1_000_000,
        "{}",
        i.usage.analysis_work
    );
    // Each capturing its own value, they are classified apart, under the
    // cumulative work allowance.
    assert_eq!(
        why(import(&many(600, true))),
        LuaWhy::Limit(LuaLimit::AnalysisWork)
    );
}

#[test]
fn classification_sees_cancellation() {
    let d = folder(&many(600, true));
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    // Cancelled once the configured pass has converted its table: only
    // the analysis's own polling can see it before its work cap would.
    let remote = cancel.clone();
    super::tests::at_converted(move || remote.cancel());
    assert_eq!(
        why(super::tests::play(
            &h,
            &Config::defaults(h.header()),
            &cancel
        )),
        LuaWhy::Cancelled
    );
}

/// A skin whose configured pass gives `skin_config.option` `keys`
/// integer entries named by `bytes`-byte names sharing all but their
/// last digits, then makes `count` closures that each read the last name
/// through `_ENV`: through one `_ENV` of their own each when `envs`, else
/// through the globals, each capturing its own index.
fn options(keys: usize, bytes: usize, count: usize, envs: bool) -> String {
    let env = if envs {
        "local _ENV = { skin_config = skin_config }\n"
    } else {
        ""
    };
    format!(
        "local s = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return s end\n\
         local p = \"x\"\n\
         while #p < {bytes} do p = p .. p end\n\
         p = string.sub(p, 1, {bytes} - 8)\n\
         local last\n\
         for i = 1, {keys} do last = p .. string.format(\"%08d\", i) skin_config.option[last] = i end\n\
         s.customEvents = {{}}\n\
         for i = 1, {count} do\n\
         local own = i\n\
         {env}\
         s.customEvents[i] = {{ action = function() local mine = own return skin_config.option[\"{{LAST}}\"] == 1 end }}\n\
         end\n\
         return s"
    )
}

/// [`options`] with the read name spelled out.
fn options_entry(keys: usize, bytes: usize, count: usize, envs: bool) -> String {
    let last = format!("{}{keys:08}", "x".repeat(bytes - 8));
    options(keys, bytes, count, envs).replace("{LAST}", &last)
}

#[test]
fn option_tables_are_copied_by_path_once_a_pass() {
    // 100 `_ENV` tables over one option table of 600 names of 6 000
    // bytes: of the option table, only the entry a path reads is copied,
    // once, its name interned once; each instance's path hashes it.
    let def = super::root_tests::configured(&options_entry(600, 6_000, 100, true));
    let an = super::root_tests::analysis(&def);
    // The 100 `_ENV` tables, `skin_config` and its `option`.
    assert_eq!(an.tables.len(), 102);
    let option = an.tables.iter().filter(|t| t.len() == 1).count();
    assert!(option >= 1);
    let copied: usize = an.tables.iter().map(std::collections::HashMap::len).sum();
    // `skin_config` in each `_ENV`, `option` once, the read entry once.
    assert_eq!(copied, 102);
    assert_eq!(an.names.len(), 3);
    // Each instance hashes the name copying its path: 100 times, never
    // once each of the 600 entries.
    let work = an.work.get();
    assert!((100 * 6_000..2 * 100 * 6_000).contains(&work), "{work}");
}

#[test]
fn option_lookups_hash_the_name_and_charge_its_bytes() {
    // 2 000 closures with distinct captures, each classified apart, each
    // looking up one of 600 names of 1 000 bytes sharing their first 992:
    // one hash a lookup, its 1 000 bytes charged, never a scan.
    let start = std::time::Instant::now();
    let i = import(&options_entry(600, 1_000, 2_000, false)).unwrap();
    assert!(
        i.usage.analysis_work >= 2_000 * 1_000,
        "{}",
        i.usage.analysis_work
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(20));
}

/// A binary tree of 16 383 helper closures, `root`, 8 192 leaves each
/// capturing its own index, under 32 numbers drawing `value`.
pub(super) fn helper_tree(value: &str) -> String {
    let body = format!(
        "local level = {{}}\n\
        for i = 1, 8192 do local k = i level[i] = function() local own = k return 0 end end\n\
        while #level > 1 do\n\
          local up = {{}}\n\
          for i = 1, #level, 2 do\n\
            local left, right = level[i], level[i + 1]\n\
            up[#up + 1] = function() local x = left() local y = right() return 0 end\n\
          end\n\
          level = up\n\
        end\n\
        local root = level[1]\n\
        skin.value = {{ {{ id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 3,\n\
          value = {value} }} }}"
    );
    let dst: String = (0..32)
        .map(|k| format!("{{ id = \"v\", dst = {{ {{ x = {k}, y = 0, w = 6, h = 6 }} }} }},"))
        .collect();
    skin(&body, &dst)
}

#[test]
fn the_role_scan_visits_each_helper_once() {
    // Under 32 numbers whose callback does not convert: each one's role
    // scan visits its value and every helper once, a hash a check. The
    // tree is called behind a condition folded false, which evaluation
    // never enters and the scan reads.
    let entry =
        helper_tree("function() if false then local unused = root() end return ms.number(425) end");
    let start = std::time::Instant::now();
    crate::import::callback::SCANNED.with(|n| n.set(0));
    let i = import(&entry).unwrap();
    assert!(
        i.callbacks
            .iter()
            .any(|d| matches!(d, super::Disposition::Replaced(_)))
    );
    // 32 scans, each of the value and the 16 383 helpers once.
    let scanned = crate::import::callback::SCANNED.with(Cell::get);
    assert_eq!(scanned, 32 * (1 + 16_383));
    assert!(start.elapsed() < std::time::Duration::from_secs(20));
}

#[test]
fn a_callable_s_module_is_found_by_hash() {
    // 1 000 modules with long names sharing all but their last digits,
    // then 2 000 closures from the last: each closure's provenance is one
    // hash of its module's name, its bytes charged.
    let prefix = "m".repeat(120);
    let mut files: Vec<(String, String)> = (0..1_000)
        .map(|k| (format!("{prefix}{k:04}.lua"), "return 1".to_owned()))
        .collect();
    files.push((
        format!("{prefix}last.lua"),
        "local t = {} for i = 1, 2000 do local k = i t[i] = function() local own = k return 0 end end return t"
            .to_owned(),
    ));
    let loads: String = (0..1_000)
        .map(|k| format!("require(\"{prefix}{k:04}\")\n"))
        .collect();
    let entry = format!(
        "local s = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return s end\n\
         {loads}s.extra = require(\"{prefix}last\")\n\
         return s"
    );
    let named: Vec<(&str, &str)> = files
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();
    let i = run_with(&entry, &named, &Cancel::new()).unwrap();
    let name = u64::try_from(prefix.len() + "last.lua".len()).unwrap();
    assert!(
        i.usage.analysis_work >= 2_000 * name,
        "{}",
        i.usage.analysis_work
    );
}

#[test]
fn a_table_met_again_is_not_scanned_again() {
    // An emptied table of 262 144 slots, referenced 100 000 times: its
    // empty capacity is walked once, not once a reference.
    let entry = "local s = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
        if not skin_config then return s end\n\
        local t = {}\n\
        for i = 1, 262144 do t[i] = true end\n\
        for i = 1, 262144 do t[i] = nil end\n\
        s.extra = {}\n\
        for i = 1, 100000 do s.extra[i] = t end\n\
        return s";
    let start = std::time::Instant::now();
    import(entry).unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(20));
}

#[test]
fn rewritten_chunks_share_one_allowance() {
    // 80 modules, each a 16 KiB name read by the innermost of 60 nested
    // functions: each of the 60 readers names it, about 960 KiB a module,
    // 77 MiB kept in all, past the pass's 64 MiB.
    let name = format!("n{}", "x".repeat(16 * 1024));
    let mut module = format!("local {name} = 1\n");
    for _ in 0..60 {
        module.push_str("local function f() ");
    }
    module.push_str(&format!("return {name} "));
    module.push_str(&"end ".repeat(60));
    module.push_str("\nreturn 1");
    let files: Vec<(String, String)> = (0..80)
        .map(|k| (format!("m{k:03}.lua"), module.clone()))
        .collect();
    let loads: String = (0..80).map(|k| format!("require(\"m{k:03}\")\n")).collect();
    let entry = format!(
        "local s = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return s end\n\
         {loads}return s"
    );
    let named: Vec<(&str, &str)> = files
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();
    assert_eq!(
        why(run_with(&entry, &named, &Cancel::new())),
        LuaWhy::Limit(LuaLimit::Generated)
    );
}

#[test]
fn a_lowering_s_memo_holds_only_what_it_emits() {
    // 60 000 callable roots nothing consumes (classified, never
    // consumed), then one number callback read by 4 000 destinations:
    // each lowering holds a memo slot for the one node it emits, not one
    // for every instance.
    let body = "skin.customEvents = {}\n\
        for i = 1, 60000 do skin.customEvents[i] = { id = 1000 + i, action = function() return 0 end } end\n\
        skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 1,\n\
          value = function() return ms.number(110) end } }";
    let dst: String = (0..4_000)
        .map(|k| {
            format!(
                "{{ id = \"v\", dst = {{ {{ x = {}, y = 0, w = 6, h = 6 }} }} }},",
                k % 1_000
            )
        })
        .collect();
    super::tests::MEMO_SLOTS.with(|n| n.set(0));
    let i = import(&skin(body, &dst)).unwrap();
    assert_eq!(
        i.callbacks
            .iter()
            .filter(|d| **d == super::Disposition::ClassifiedOnly)
            .count(),
        60_000
    );
    let slots = super::tests::MEMO_SLOTS.with(Cell::get);
    assert_eq!(slots, 4_000, "{slots} memo slots");
}

/// The model fields read by the counts the oracle sets, by property id:
/// written from the play schema's names, apart from the importer.
const FIELDS: [(i64, &str); 9] = [
    (106, "notes"),
    (110, "count.perfect"),
    (111, "count.great"),
    (112, "count.good"),
    (113, "count.bad"),
    (114, "count.poor"),
    (410, "count.early.perfect"),
    (411, "count.late.perfect"),
    (101, "ex_score"),
];

/// The oracle: the skin's configured construction replayed in a plain VM
/// with test host providers, its callbacks called as the source calls
/// them.
struct Oracle {
    lua: mlua::Lua,
    callbacks: Vec<mlua::Function>,
}

impl Oracle {
    fn new(entry: &str) -> Self {
        let lua = mlua::Lua::new();
        let setup = "values, kind = {}, 0\n\
            local ms = { timer_off_value = math.mininteger }\n\
            function ms.number(id) return values[id] end\n\
            function ms.gauge_type() return kind end\n\
            skin_config = { option = {} }\n\
            local plain = require\n\
            require = function(n) if n == \"main_state\" then return ms end return plain(n) end";
        lua.load(setup).exec().unwrap();
        let skin: mlua::Table = lua.load(entry).eval().unwrap();
        let mut callbacks = Vec::new();
        let dst: mlua::Table = skin.get("destination").unwrap();
        for d in dst.sequence_values::<mlua::Table>() {
            if let Ok(f) = d.unwrap().get::<mlua::Function>("draw") {
                callbacks.push(f);
            }
        }
        if let Ok(values) = skin.get::<mlua::Table>("value") {
            for v in values.sequence_values::<mlua::Table>() {
                callbacks.push(v.unwrap().get("value").unwrap());
            }
        }
        Self { lua, callbacks }
    }

    /// Each callback's result with `values` read and gauge type `kind`.
    fn results(&self, values: &HashMap<i64, i64>, kind: i64) -> Vec<i64> {
        let t: mlua::Table = self.lua.globals().get("values").unwrap();
        for (&id, &v) in values {
            t.set(id, v).unwrap();
        }
        self.lua.globals().set("kind", kind).unwrap();
        self.callbacks
            .iter()
            .map(|f| match f.call::<mlua::Value>(()).unwrap() {
                mlua::Value::Boolean(b) => i64::from(b),
                mlua::Value::Integer(n) => n,
                other => panic!("{other:?}"),
            })
            .collect()
    }
}

/// The lowered expressions' results with the same reads.
fn lowered(
    programs: &[(skin_model::Program, Ty)],
    values: &HashMap<i64, i64>,
    kind: i64,
) -> Vec<i64> {
    let mut fields = vec![0_i32; PLAY.fields.len()];
    for (id, name) in FIELDS {
        let (slot, _) = PLAY.field(name).unwrap();
        fields[usize::from(slot)] = i32::try_from(values[&id]).unwrap();
    }
    let (kind_slot, _) = PLAY.field("gauge.kind").unwrap();
    let (valid_slot, _) = PLAY.field("gauge.valid").unwrap();
    fields[usize::from(kind_slot)] = i32::try_from(kind).unwrap();
    fields[usize::from(valid_slot)] = 1;
    let timers = vec![-1_i32; PLAY.timers.len()];
    let state = State {
        fields: &fields,
        timers: &timers,
    };
    programs
        .iter()
        .map(|(p, ty)| match ty {
            Ty::Bool => i64::from(p.eval_bool(&state)),
            _ => i64::from(p.eval(&state)),
        })
        .collect()
}

#[test]
fn lowered_callbacks_agree_with_the_closures_they_come_from() {
    let body = "local function mk(k) return function() return ms.number(110) > k end end\n\
        local same_a, same_b = function() return ms.number(410) == ms.number(411) end, function() return ms.number(411) > ms.number(410) end\n\
        local function at_least_one() local l = ms.number(411) return l >= 1 end";
    let dst = format!(
        "{}{}{}{}{}{}{}{}{}",
        image("mk(0)"),
        image("mk(1)"),
        image("mk(2)"),
        image("same_a"),
        image("same_b"),
        image(
            "function() local t = ms.number(110) local e = ms.number(410) local l = ms.number(411) \
             return (e > l and at_least_one()) or t == e end"
        ),
        image("function() return not (ms.number(111) - ms.number(112) * 3 <= -2) end"),
        image("function() local k = ms.gauge_type() return k == 4 or k ~= 1 and k == 5 end"),
        "{ id = \"v\", dst = { { x = 0, y = 100, w = 6, h = 6 } } },"
    );
    let value = "skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8,\n\
        value = function() local run = ms.number(110) + ms.number(111) + ms.number(112) + ms.number(113) + ms.number(114)\n\
          return ms.number(106) - run * 2 + 7 - ms.number(101) end } }";
    let entry = skin(&format!("{body}\n{value}"), &dst);
    LOWERED.with(|l| l.borrow_mut().clear());
    let i = import(&entry).unwrap();
    assert!(
        i.callbacks
            .iter()
            .all(|d| *d == super::Disposition::Lowered),
        "{:?}",
        i.callbacks
    );
    let sources = LOWERED.with(|l| l.borrow().clone());
    assert_eq!(sources.len(), 9);
    let programs: Vec<(skin_model::Program, Ty)> = sources
        .iter()
        .enumerate()
        .map(|(k, src)| {
            let ty = if k < 8 { Ty::Bool } else { Ty::Int };
            (compile(src, &Scope::new(&PLAY), ty).unwrap(), ty)
        })
        .collect();
    let oracle = Oracle::new(&entry);
    // Each read's range extremes and the 0, 1, 2 boundaries, asymmetric.
    let max = i64::from(skin_model::MAX_COUNT);
    let grid = [0, 1, 2, 3, max - 1, max];
    let mut checked = 0;
    for &a in &grid {
        for &b in &grid {
            for &c in &grid {
                for kind in 0..6 {
                    let values: HashMap<i64, i64> = FIELDS
                        .iter()
                        .enumerate()
                        .map(|(n, &(id, _))| {
                            let v = match id {
                                110 => a,
                                410 => b,
                                411 => c,
                                _ => (a + 2 * b + 3 * c + i64::try_from(n).unwrap()) % (max + 1),
                            };
                            (id, v)
                        })
                        .collect();
                    assert_eq!(
                        lowered(&programs, &values, kind),
                        oracle.results(&values, kind),
                        "{values:?} kind {kind}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(checked, 6 * 6 * 6 * 6);
}

type Captures = Vec<super::callback::Captured>;
type Lowerings = Result<(Vec<super::Disposition>, Vec<String>), ImportError>;

/// The configured pass of `entry` against `facts`: its captured values,
/// then the import's callbacks and the sources it lowered, or its
/// refusal.
fn with_facts(entry: &str, facts: &super::facts::ImportFacts) -> (Captures, Lowerings) {
    let d = folder(entry);
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let config = Config::defaults(h.header());
    let (def, counts, store, reads) = h.configured(&config, facts, &cancel);
    let def = def.unwrap();
    let an = def.arena.lua.as_deref().and_then(|l| l.analysis.as_deref());
    let captured = an.unwrap().captured.clone();
    LOWERED.with(|l| l.borrow_mut().clear());
    let o = h.finish(Ok(def), counts, store, reads, &config);
    let r = match o.result {
        Ok(_) => Ok((o.callbacks, LOWERED.with(|l| l.borrow().clone()))),
        Err(super::ledger::Stop::Fault(e)) => Err(e),
        Err(super::ledger::Stop::Pending(p)) => panic!("{p:?}"),
    };
    (captured, r)
}

/// A play snapshot with every fact known.
fn known() -> super::facts::ImportFacts {
    use super::facts::{ChartRank, Fact, ImportFacts};
    use skin_model::ScreenKind;
    ImportFacts {
        scene: ScreenKind::Play,
        autoplay: Fact::Known(true),
        chart_rank: Fact::Known(Some(ChartRank::Rank(2))),
        play_level: Fact::Known(12),
    }
}

#[test]
fn a_callback_keeps_the_option_host_and_lowers_a_supported_option() {
    use super::callback::Captured;
    // A closure holding `main_state.option` captures the host function by
    // identity (no conversion takes it as a value: the callback refuses).
    let entry = skin(
        "local option = ms.option",
        &image("function() local f = option return true end"),
    );
    let (captured, _) = with_facts(&entry, &known());
    assert!(captured.contains(&Captured::Host(0)), "{captured:?}");
    // A read of a supported option lowers to its expression.
    let entry = skin("", &image("function() return ms.option(271) end"));
    let (_, r) = with_facts(&entry, &known());
    let (callbacks, sources) = r.unwrap();
    assert_eq!(callbacks, [super::Disposition::Lowered]);
    let [src] = sources.as_slice() else {
        panic!("{sources:?}")
    };
    let program = compile(src, &Scope::new(&PLAY), Ty::Bool).unwrap();
    let (slot, _) = PLAY.field("cover.enabled").unwrap();
    let timers = vec![-1_i32; PLAY.timers.len()];
    for on in [false, true] {
        let mut fields = vec![0_i32; PLAY.fields.len()];
        fields[usize::from(slot)] = i32::from(on);
        let state = State {
            fields: &fields,
            timers: &timers,
        };
        assert_eq!(program.eval_bool(&state), on);
    }
}

#[test]
fn a_callback_s_fact_read_is_not_folded() {
    let entry = skin("", &image("function() return ms.option(33) end"));
    let unmapped = super::Disposition::Replaced(Unsupported::Unmapped { api: 0, id: 33 });
    let unavailable = super::facts::ImportFacts::unavailable(skin_model::ScreenKind::Play);
    for facts in [unavailable, known()] {
        let (_, r) = with_facts(&entry, &facts);
        let (callbacks, sources) = r.unwrap();
        assert_eq!(callbacks, [unmapped], "{facts:?}");
        assert!(sources.is_empty(), "{sources:?}");
    }
}
