//! The callbacks' refusals, bounds and differential oracle, which need
//! the crate's private witnesses: the host calls made while captures are
//! copied ([`super::tests::CAPTURE_CALLS`]), the lowered sources
//! ([`crate::import::callback::LOWERED`]) and a cancellation at an
//! expansion ([`super::tests::at_expansion`]).

use std::cell::Cell;
use std::collections::HashMap;

use skin_model::{PLAY, Scope, State, Ty, compile};

use super::tests::CAPTURE_CALLS;
use super::{Cancel, LuaLimit, LuaWhy, Unsupported, read_lua_header};
use crate::error::ImportError;
use crate::import::Config;
use crate::import::callback::LOWERED;

/// A skin folder whose `s.luaskin` is `entry`, with a 64×64 `a.png`.
fn folder(entry: &str) -> tempfile::TempDir {
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
fn skin(body: &str, dst: &str) -> String {
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
fn image(draw: &str) -> String {
    format!("{{ id = \"i\", draw = {draw}, dst = {{ {{ x = 0, y = 0, w = 64, h = 64 }} }} }},")
}

/// A gauge destination drawn under `draw`: critical.
fn gauge(draw: &str) -> String {
    format!("{{ id = \"g\", draw = {draw}, dst = {{ {{ x = 100, y = 500, w = 100, h = 10 }} }} }},")
}

fn run(entry: &str, cancel: &Cancel) -> Result<super::LuaImported, ImportError> {
    run_with(entry, &[], cancel)
}

/// [`run`] with `files` beside the entry.
fn run_with(
    entry: &str,
    files: &[(&str, &str)],
    cancel: &Cancel,
) -> Result<super::LuaImported, ImportError> {
    let d = folder(entry);
    for (name, text) in files {
        std::fs::write(d.path().join(name), text).unwrap();
    }
    let h = read_lua_header(d.path(), "s.luaskin", cancel)?;
    h.import(&Config::defaults(h.header()), cancel)
}

/// Imports `entry` and checks that copying its captures made no host
/// call, so ran no skin handler.
fn import(entry: &str) -> Result<super::LuaImported, ImportError> {
    CAPTURE_CALLS.with(|n| n.set(0));
    let r = run(entry, &Cancel::new());
    assert_eq!(CAPTURE_CALLS.with(Cell::get), 0, "{entry}");
    r
}

#[track_caller]
fn why<T: std::fmt::Debug>(r: Result<T, ImportError>) -> LuaWhy {
    match r {
        Err(ImportError::Lua { why, .. }) => why,
        other => panic!("{other:?}"),
    }
}

/// A test handler: any skin code a capture ran would call it, a host
/// call (`pcall`'s wrapper charges one).
const PROBE: &str = "local function probe() pcall(type, 1) end";

#[test]
fn refusals_name_their_reason_and_run_no_handler() {
    let effect = |w| LuaWhy::Effect(w);
    for (body, dst, want) in [
        // The module reassigned, or written through an alias.
        (
            "ms = { number = function() return 1 end }",
            gauge("function() return ms.number(110) > 0 end"),
            effect(Unsupported::Binding("a host module")),
        ),
        (
            "local m = ms\npcall(function() m.number = nil end)",
            gauge("function() return ms.number(110) > 0 end"),
            LuaWhy::HostWrite,
        ),
        // An impure helper.
        (
            "local n = 0\nlocal function bump() n = n + 1 return true end",
            gauge("function() return bump() end"),
            effect(Unsupported::Construct("assignment")),
        ),
        // Integer truthiness.
        (
            "",
            image("function() return ms.number(1) and true end"),
            effect(Unsupported::Truthiness),
        ),
        // Recursion.
        (
            "local f\nlocal function g() return f() end\nf = function() return g() end",
            image("function() return g() end"),
            effect(Unsupported::Binding("a helper")),
        ),
        (
            "local function r() return r() end",
            image("function() return r() end"),
            effect(Unsupported::Recursion),
        ),
        // `/` in a comparison.
        (
            "",
            image("function() return ms.number(110) / 2 > 0 end"),
            effect(Unsupported::Construct("/")),
        ),
        // A global read.
        (
            "",
            image("function() return flag end"),
            effect(Unsupported::Global),
        ),
        // An upvalue mutated by the callback.
        (
            "local n = 0",
            image("function() n = n + 1 return true end"),
            effect(Unsupported::Construct("assignment")),
        ),
        // A decorative writer beside a critical gauge callback.
        (
            "",
            format!(
                "{}{}",
                image("function() ms.set_timer(1, 0) return true end"),
                gauge("function() return ms.gauge_type() == 4 end")
            ),
            effect(Unsupported::Construct("call statement")),
        ),
        (
            "",
            image("function() return ms.set_timer(1, 0) end"),
            effect(Unsupported::HostApi(12)),
        ),
        // A forbidden string operation.
        (
            "",
            image("function() return (\"\").x end"),
            effect(Unsupported::Construct("index")),
        ),
        // A table comparison.
        (
            "",
            image("function() return {} == {} end"),
            effect(Unsupported::Construct("table")),
        ),
        // `<close>` and a generic `for`.
        (
            "",
            image("function() local x <close> = nil return true end"),
            effect(Unsupported::Construct("<close>")),
        ),
        (
            "",
            image("function() for k in pairs({}) do end return true end"),
            effect(Unsupported::Construct("generic for")),
        ),
        // `_ENV` with a metatable; `skin_config.option` with one, or
        // without the raw entry.
        (
            &format!("{PROBE}\nsetmetatable(_ENV, {{ __index = function() probe() end }})"),
            image("function() return skin_config.option[\"x\"] == 1 end"),
            effect(Unsupported::Capture("_ENV's metatable")),
        ),
        (
            &format!(
                "{PROBE}\nsetmetatable(skin_config.option, {{ __index = function() probe() return 1 end }})"
            ),
            image("function() return skin_config.option[\"x\"] == 1 end"),
            effect(Unsupported::Capture("skin_config.option's metatable")),
        ),
        (
            "",
            image("function() return skin_config.option[\"x\"] == 1 end"),
            effect(Unsupported::Capture("an option entry")),
        ),
        (
            "skin_config.option.x = \"1\"",
            image("function() return skin_config.option[\"x\"] == 1 end"),
            effect(Unsupported::Capture("an option entry")),
        ),
        // `skin_config` replaced.
        (
            "skin_config = { option = { x = 1 } }",
            image("function() return skin_config.option[\"x\"] == 1 end"),
            effect(Unsupported::Capture("skin_config")),
        ),
        // A float.
        (
            "",
            image("function() return ms.number(110) > 1.0 end"),
            effect(Unsupported::Construct("float")),
        ),
    ] {
        let entry = skin(body, &dst);
        assert_eq!(why(import(&entry)), want, "{body} / {dst}");
    }
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
    // Classification, at admission, of a function nothing consumes: a
    // left-leaning chain the parser reads without recursing.
    let chain = vec!["1"; 100_000].join("+");
    let entry = skin(
        &format!("skin.extra = function() return {chain} == 0 end"),
        "",
    );
    assert_eq!(why(import(&entry)), fatal);
    // Classification through helpers, each calling the one before.
    let mut helpers = String::from("local function h0() return ms.number(110) > 0 end\n");
    for k in 1..150 {
        helpers.push_str(&format!(
            "local function h{k}() return h{}() and true end\n",
            k - 1
        ));
    }
    let entry = skin(&helpers, &image("function() return h149() end"));
    assert_eq!(why(import(&entry)), fatal);
    // Lowering, which inlines each local where it is read: shallow for
    // classification, deep once inlined.
    let mut locals = String::from("local a0 = ms.number(110) ");
    for k in 1..150 {
        locals.push_str(&format!("local a{k} = a{} + 1 ", k - 1));
    }
    let entry = skin(
        "",
        &image(&format!("function() {locals} return a149 > 0 end")),
    );
    assert_eq!(why(import(&entry)), fatal);
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
         s.extra = {{}}\n\
         for i = 1, {count} do local k = i s.extra[i] = function()\n\
         {own}{body}return true\n\
         end end\n\
         return s"
    )
}

#[test]
fn a_function_made_in_a_loop_is_classified_once() {
    // 60 000 closures of one function, nothing captured: its body is
    // walked once.
    let i = import(&many(60_000, false)).unwrap();
    assert_eq!(i.callbacks.len(), 60_000);
    // One unit a callable, and the body once.
    assert!(i.usage.analysis_work < 150_000, "{}", i.usage.analysis_work);
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
        why(h.import(&Config::defaults(h.header()), &cancel)),
        LuaWhy::Cancelled
    );
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
