//! Lua skins through the public entries: every test runs
//! [`read_lua_header`] (the header pass) and, where it imports,
//! [`LuaHeader::import`] (the configured pass, the conversion and the
//! common import). Every fixture is a small self-made skin.

mod common;

use skin_beatoraja::limits::{MAX_FILE_BYTES, MAX_INCLUDE_DEPTH, MAX_INCLUDE_PARSES, MAX_NODES};
use skin_beatoraja::lua::HOST_APIS;
use skin_beatoraja::lua::limits::{MAX_HOST_CALLS, MAX_INSTRUCTIONS};
use skin_beatoraja::{
    Cancel, Config, ImportError, InputLimit, LuaHeader, LuaImported, LuaLimit, LuaWhy, NoteKind,
    OffsetValue, read_lua_header,
};
use skin_model::RootError;
use tempfile::TempDir;

/// A skin folder with `files` and a 64×64 `a.png`.
fn folder(files: &[(&str, &str)]) -> TempDir {
    let d = common::skin(files);
    common::png(d.path(), "a.png", 64, 64);
    d
}

/// The header pass of `s.luaskin`.
fn header(d: &TempDir) -> Result<LuaHeader, ImportError> {
    read_lua_header(d.path(), "s.luaskin", &Cancel::new())
}

/// Both passes of `s.luaskin` with `config` made from the header.
fn import_with(
    d: &TempDir,
    config: impl FnOnce(&LuaHeader) -> Config,
) -> Result<LuaImported, ImportError> {
    let h = header(d)?;
    h.import(&config(&h), &Cancel::new())
}

/// Both passes of `s.luaskin` with the default configuration.
fn import(d: &TempDir) -> Result<LuaImported, ImportError> {
    import_with(d, |h| Config::defaults(h.header()))
}

/// A play skin: the header's `fields`, then `body` run in the configured
/// pass only, with `skin` the header table holding `a.png` as source `a`
/// and its whole as image `i`.
fn skin(fields: &str, body: &str) -> String {
    format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080, {fields} }}\n\
         if not skin_config then return skin end\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }} }}\n\
         {body}\n\
         return skin\n"
    )
}

/// [`skin`] drawing image `i` once with `fields` in its destination.
fn drawn(fields: &str) -> String {
    skin(
        "",
        &format!(
            "skin.destination = {{ {{ id = \"i\", {fields} dst = {{ {{ x = 0, y = 0, w = 64, h = 64 }} }} }} }}"
        ),
    )
}

/// A folder whose `s.luaskin` runs `body` in the configured pass of a
/// skin drawing image `i`.
fn configured(body: &str) -> TempDir {
    folder(&[("s.luaskin", &skin("", &format!("{body}\n{}", DRAW_I)))])
}

const DRAW_I: &str =
    "skin.destination = { { id = \"i\", dst = { { x = 0, y = 0, w = 64, h = 64 } } } }";

/// The Lua refusal of `r`.
#[track_caller]
fn why<T: std::fmt::Debug>(r: Result<T, ImportError>) -> LuaWhy {
    match r {
        Err(ImportError::Lua { why, .. }) => why,
        other => panic!("{other:?}"),
    }
}

/// The Lua refusal of `r` and its location's file and line.
#[track_caller]
fn located<T: std::fmt::Debug>(r: Result<T, ImportError>) -> (LuaWhy, String, u32) {
    match r {
        Err(ImportError::Lua { why, at: Some(at) }) => (why, at.file, at.line),
        other => panic!("{other:?}"),
    }
}

/// The input bound `r` stopped at.
#[track_caller]
fn input<T: std::fmt::Debug>(r: Result<T, ImportError>) -> InputLimit {
    match r {
        Err(ImportError::Input { limit, .. }) => limit,
        other => panic!("{other:?}"),
    }
}

/// The read refusal of `r`.
#[track_caller]
fn read<T: std::fmt::Debug>(r: Result<T, ImportError>) -> RootError {
    match r {
        Err(ImportError::Read { source, .. }) => source,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_data_only_skin_imports_through_the_common_mapper() {
    let d = folder(&[("s.luaskin", &drawn(""))]);
    let r = import(&d).unwrap();
    assert_eq!(r.dependencies, ["s.luaskin"]);
    assert_eq!(r.imported.report.sources, ["s.luaskin"]);
    assert_eq!(r.imported.report.usage.elements, 1);
    assert!(
        r.imported.report.notes.is_empty(),
        "{:?}",
        r.imported.report.notes
    );
}

#[test]
fn the_environment_is_an_allowlist() {
    let absent = [
        "io",
        "os",
        "debug",
        "package",
        "load",
        "loadfile",
        "loadstring",
        "collectgarbage",
        "print",
        "warn",
        "coroutine",
        "utf8",
        "module",
        "string.dump",
        "string.pack",
        "string.unpack",
        "string.packsize",
        "math.random",
        "math.randomseed",
    ];
    let present = [
        "assert",
        "error",
        "getmetatable",
        "ipairs",
        "next",
        "pairs",
        "pcall",
        "rawequal",
        "rawget",
        "rawlen",
        "rawset",
        "select",
        "setmetatable",
        "tonumber",
        "tostring",
        "type",
        "xpcall",
        "require",
        "dofile",
        "_G",
        "_VERSION",
        "string.find",
        "string.format",
        "table.concat",
        "table.sort",
        "math.floor",
        "math.mininteger",
    ];
    let lookup = |name: &str| match name.split_once('.') {
        Some((lib, f)) => format!("(_G[\"{lib}\"] or {{}})[\"{f}\"]"),
        None => format!("_G[\"{name}\"]"),
    };
    let mut src = String::new();
    for name in absent {
        src += &format!(
            "if {} ~= nil then error(\"{name} is present\") end\n",
            lookup(name)
        );
    }
    for name in present {
        src += &format!(
            "if {} == nil then error(\"{name} is absent\") end\n",
            lookup(name)
        );
    }
    src += "if _G ~= _ENV then error(\"_G\") end\nreturn { type = 0 }\n";
    let d = folder(&[("s.luaskin", &src)]);
    header(&d).unwrap();
}

#[test]
fn a_coroutine_is_unavailable() {
    let d = folder(&[(
        "s.luaskin",
        "local co = coroutine.create(function() while true do end end)\n\
         coroutine.resume(co)\nreturn { type = 0 }",
    )]);
    assert_eq!(located(header(&d)), (LuaWhy::Error, "s.luaskin".into(), 1));
}

#[test]
fn an_infinite_loop_reaches_the_instruction_limit() {
    let d = folder(&[("s.luaskin", "while true do end")]);
    assert_eq!(why(header(&d)), LuaWhy::Limit(LuaLimit::Instructions));
}

#[test]
fn exhaustion_inside_a_protected_call_stays_fatal() {
    for body in [
        "pcall(function() while true do end end)",
        "for i = 1, 100 do pcall(function() while true do end end) end",
        "xpcall(function() while true do end end, function(e) return e end)",
        "local ok = pcall(pcall, function() while true do end end)\nif not ok then return { type = 0 } end",
    ] {
        let d = folder(&[("s.luaskin", &format!("{body}\nreturn {{ type = 0 }}"))]);
        assert_eq!(
            why(header(&d)),
            LuaWhy::Limit(LuaLimit::Instructions),
            "{body}"
        );
    }
}

#[test]
fn the_instruction_limit_counts_both_passes() {
    // One instruction per iteration of a numeric `for`: under the cap the
    // skin imports, over it the configured pass is refused.
    let under = configured("for i = 1, 9900000 do end");
    let r = import(&under).unwrap();
    assert!(
        r.usage.instructions <= MAX_INSTRUCTIONS,
        "{}",
        r.usage.instructions
    );
    assert!(
        r.usage.instructions >= 9_900_000,
        "{}",
        r.usage.instructions
    );
    let over = configured("for i = 1, 10000000 do end");
    assert_eq!(why(import(&over)), LuaWhy::Limit(LuaLimit::Instructions));
    // Most of the cap in each pass is over it in all: the configured pass
    // starts from what the header pass used.
    let halves = folder(&[(
        "s.luaskin",
        &format!(
            "if not skin_config then for i = 1, 6000000 do end end\n{}",
            skin("", &format!("for i = 1, 6000000 do end\n{DRAW_I}"))
        ),
    )]);
    let h = header(&halves).unwrap();
    assert!(h.usage().instructions >= 6_000_000, "{:?}", h.usage());
    assert_eq!(
        why(h.import(&Config::defaults(h.header()), &Cancel::new())),
        LuaWhy::Limit(LuaLimit::Instructions)
    );
}

#[test]
fn memory_is_capped_before_it_is_allocated() {
    // 24 MiB live, built by the VM's own concatenation, fit in the 64 MiB.
    let fits = configured("local s = \"x\"\nfor i = 1, 24 do s = s .. s end");
    import(&fits).unwrap();
    for body in [
        "local s = \"x\"\nfor i = 1, 40 do s = s .. s end",
        "local s = string.rep(\"x\", 1 << 20)\nlocal t = {}\nfor i = 1, 100 do t[i] = s .. i end",
        "pcall(function() local s = \"x\" for i = 1, 40 do s = s .. s end end)",
        // Inside the library: its own buffers exhaust memory.
        "local s = string.rep(\"x\", 1 << 19)\nlocal t = {}\n\
         pcall(function() for i = 1, 160 do t[i] = string.upper(s .. i) end end)",
    ] {
        let d = configured(body);
        assert_eq!(why(import(&d)), LuaWhy::Limit(LuaLimit::Memory), "{body}");
    }
}

#[test]
fn recursion_ends_in_a_located_error() {
    let d = folder(&[(
        "s.luaskin",
        "local function f(n) return 1 + f(n + 1) end\nf(0)\nreturn { type = 0 }",
    )]);
    assert_eq!(located(header(&d)), (LuaWhy::Error, "s.luaskin".into(), 1));
    // A metamethod that recurses through C.
    let d = folder(&[(
        "s.luaskin",
        "local t = setmetatable({}, { __index = function(t, k) return t[k] end })\n\
         local x = t.a\nreturn { type = 0 }",
    )]);
    assert_eq!(why(header(&d)), LuaWhy::Error);
}

#[test]
fn host_calls_are_capped_across_protected_calls() {
    // A protected call settles through a host function: one call each.
    let n = MAX_HOST_CALLS;
    let at = configured(&format!("for i = 1, {n} do pcall(type, i) end"));
    let r = import(&at).unwrap();
    assert_eq!(r.usage.host_calls, n);
    let over = configured(&format!("for i = 1, {} do pcall(type, i) end", n + 1));
    assert_eq!(why(import(&over)), LuaWhy::Limit(LuaLimit::HostCalls));
    let caught = configured(&format!(
        "pcall(function() for i = 1, {} do pcall(type, i) end end)",
        n + 1
    ));
    assert_eq!(why(import(&caught)), LuaWhy::Limit(LuaLimit::HostCalls));
}

#[test]
fn the_library_runs_its_loops_as_metered_lua() {
    // The forms the owner's skins use work as Lua's do.
    let fine = configured(
        "assert(string.find(\"a.png\", \"%.png$\") == 2)\n\
         assert(select(2, string.gsub(\"a/b/c\", \"[/\\\\]\", \"-\")) == 2)\n\
         assert(string.gsub(\"abc\", \"(%w)\", \"%1%1\") == \"aabbcc\")\n\
         assert(string.match(\"skin/play/x.lua\", \"(.+)/\") == \"skin/play\")\n\
         assert(string.match(\"key=val\", \"^(%a+)=(%a+)$\") == \"key\")\n\
         local n = 0 for w in string.gmatch(\"one two  three\", \"%a+\") do n = n + 1 end\n\
         assert(n == 3)\n\
         local g = 0 for w in string.gmatch(\"^a^a\", \"^a\") do g = g + 1 end\n\
         assert(g == 2, \"a leading ^ is a literal in gmatch\")\n\
         assert(string.find(\"a+b\", \"+\", 1, true) == 2)\n\
         assert(string.format(\"%02d:%s\", 7, \"x\") == \"07:x\")\n\
         assert(tonumber(\"0x10\") == 16 and tonumber(\"10\", 2) == 2)\n\
         local t = { 3, 1, 2 }\n\
         table.sort(t) assert(t[1] == 1 and t[3] == 3)\n\
         table.sort(t, function(a, b) return a > b end) assert(t[1] == 3)\n\
         table.insert(t, 1, 9) assert(t[1] == 9 and #t == 4)\n\
         assert(table.remove(t) == 1 and #t == 3)\n\
         assert(table.concat({ 1, \"b\", 3 }, \",\") == \"1,b,3\")\n\
         assert(select(\"#\", table.unpack({ 1, 2, 3 })) == 3)\n\
         assert((\"abc\"):upper() == \"ABC\" and (\"abc\"):sub(2) == \"bc\")",
    );
    import(&fine).unwrap();
    // Backtracking and long copies are instructions: the cap ends them.
    for body in [
        "string.find(string.rep(\"a\", 3000), \"a*a*a*b\")",
        "pcall(string.gsub, string.rep(\"a\", 3000), \"(a-)(a-)(a-)b\", \"\")",
        "table.move({}, 1, 1 << 40, 1)",
        "table.move({}, 1.0, 2^40, 1.0)",
        "table.move({}, \"1\", \"1099511627776\", 1)",
        "local t = {}\nfor i = 1, 1e6 do table.insert(t, 1, i) end",
    ] {
        let d = configured(body);
        assert_eq!(
            why(import(&d)),
            LuaWhy::Limit(LuaLimit::Instructions),
            "{body}"
        );
    }
    // Native primitives: a result over the per-call cap is refused before
    // the call; small results add up to the byte budget.
    for (body, want) in [
        (
            "pcall(string.rep, \"xx\", \"1e8\")",
            LuaWhy::Library {
                op: "result over the primitive cap",
            },
        ),
        (
            "local s = string.rep(\"a\", 1 << 20)\npcall(string.sub, s .. \"b\", 1)",
            LuaWhy::Library {
                op: "result over the primitive cap",
            },
        ),
        (
            "local s = string.rep(\"1\", 1 << 20)\npcall(tonumber, s .. \"1\")",
            LuaWhy::Library {
                op: "result over the primitive cap",
            },
        ),
        (
            "local s = string.rep(\"a\", 1 << 19)\nfor i = 1, 300 do local t = s:sub(1) end",
            LuaWhy::Limit(LuaLimit::NativeWork),
        ),
        (
            "local s = string.rep(\"1\", 1 << 19)\nfor i = 1, 300 do pcall(tonumber, s) end",
            LuaWhy::Limit(LuaLimit::NativeWork),
        ),
        // A metatable the table functions would consult is refused, so
        // `__len` is never read twice.
        (
            "local k = 0\n\
             local t = setmetatable({}, { __len = function() k = k + 1 return k == 1 and 0 or 1 << 40 end })\n\
             table.insert(t, 1, false)",
            LuaWhy::Library {
                op: "table with a metatable",
            },
        ),
        (
            "pcall(table.concat, setmetatable({}, { __index = {} }))",
            LuaWhy::Library {
                op: "table with a metatable",
            },
        ),
        (
            "string.find(\"(x)\", \"%b()\")",
            LuaWhy::Library { op: "pattern %b" },
        ),
        (
            "pcall(string.match, \"aa\", \"(a)%1\")",
            LuaWhy::Library {
                op: "pattern back reference",
            },
        ),
        (
            "local s = string.rep(\"a\", 1 << 20)\nstring.format(\"%s%s\", s, s)",
            LuaWhy::Library {
                op: "result over the primitive cap",
            },
        ),
    ] {
        let d = configured(body);
        assert_eq!(why(import(&d)), want, "{body}");
    }
    // A replacement or a join that fails copies nothing: the result is
    // built only once every piece is checked.
    for body in [
        "local big = string.rep(\"b\", 1 << 19)\n\
         for i = 1, 300 do assert(not pcall(string.gsub, \"ab\", \".\", { a = big, b = {} })) end",
        "local big = string.rep(\"b\", 1 << 19)\n\
         for i = 1, 300 do assert(not pcall(table.concat, { big, big, {} })) end",
    ] {
        let r = import(&configured(body)).unwrap_or_else(|e| panic!("{body}: {e:?}"));
        assert!(r.usage.native_work < 1 << 20, "{body}: {:?}", r.usage);
    }
    // An exhausted iterator stays exhausted: each later call is a few
    // instructions, never a rescan of its subject.
    let d = configured(
        "local it = string.gmatch(string.rep(\"a\", 1 << 16), \"z\")\n\
         for i = 1, 100000 do assert(it() == nil) end",
    );
    import(&d).unwrap();
    // The natives stay private: the strings' methods are the library's.
    let d = configured(
        "assert(getmetatable(\"\").__index == string, \"methods\")\n\
         assert(string.dump == nil and (\"\").dump == nil, \"dump\")",
    );
    import(&d).unwrap();
}

#[test]
fn finalizers_and_weak_tables_are_refused() {
    for body in [
        "setmetatable({}, { __gc = function() end })",
        "setmetatable({}, { __mode = \"k\" })",
        "pcall(setmetatable, {}, { __gc = true })",
    ] {
        let d = folder(&[("s.luaskin", &format!("{body}\nreturn {{ type = 0 }}"))]);
        assert_eq!(
            located(header(&d)),
            (LuaWhy::Finalizer, "s.luaskin".into(), 1)
        );
    }
    // An ordinary metatable is fine.
    let d = folder(&[(
        "s.luaskin",
        "local t = setmetatable({}, { __index = function() return 0 end })\n\
         assert(t.x == 0)\nreturn { type = 0 }",
    )]);
    header(&d).unwrap();
}

#[test]
fn a_metatable_is_fixed_when_it_is_attached() {
    // Garbage enough for the collector to run several cycles.
    let churn = "for i = 1, 400 do local s = string.rep(\"x\", 1 << 16) .. i end";
    for (attach, change) in [
        (
            "local mt = {}\nlocal t = setmetatable({}, mt)",
            "mt.__mode = \"v\"",
        ),
        (
            "local mt = {}\nlocal t = setmetatable({}, mt)",
            "rawset(mt, \"__mode\", \"v\")",
        ),
        (
            "local mt = {}\nlocal t = setmetatable({}, mt)",
            "getmetatable(t).__mode = \"v\"",
        ),
        // A failed replacement leaves the guard in place.
        (
            "local mt = {}\nlocal t = setmetatable({}, mt)\n\
             assert(not pcall(setmetatable, t, 1))\n\
             assert(not pcall(setmetatable, 1, {}))",
            "getmetatable(t).__mode = \"v\"",
        ),
    ] {
        let d = folder(&[(
            "s.luaskin",
            &format!(
                "{attach}\n{change}\nt[1] = {{}}\n{churn}\n\
                 assert(t[1] ~= nil, \"collected\")\n\
                 assert(getmetatable(t) == mt, \"the skin's own table\")\n\
                 return {{ type = 0 }}"
            ),
        )]);
        header(&d).unwrap_or_else(|e| panic!("{change}: {e:?}"));
    }
    // The skin replaces and removes its metatable; a protection of its
    // own still holds.
    let d = folder(&[(
        "s.luaskin",
        "local t = setmetatable({}, { __index = function() return 1 end })\n\
         assert(t.x == 1)\n\
         setmetatable(t, { __index = function() return 2 end })\n\
         assert(t.x == 2)\n\
         setmetatable(t, nil)\n\
         assert(t.x == nil and getmetatable(t) == nil)\n\
         local locked = setmetatable({}, { __metatable = \"locked\" })\n\
         assert(not pcall(setmetatable, locked, {}), \"protection\")\n\
         assert(getmetatable(locked) == \"locked\")\n\
         return { type = 0 }",
    )]);
    header(&d).unwrap();
}

#[test]
fn an_error_handler_that_does_not_complete_is_fatal() {
    for (handler, want) in [
        (
            "function() local s = \"x\" for i = 1, 40 do s = s .. s end return s end",
            [LuaLimit::Memory, LuaLimit::Handler],
        ),
        (
            "function(e) error(e) end",
            [LuaLimit::Memory, LuaLimit::Handler],
        ),
        (
            "function(e) local function f() return f() + 1 end return f() end",
            [LuaLimit::Memory, LuaLimit::Handler],
        ),
    ] {
        let d = folder(&[(
            "s.luaskin",
            &format!(
                "local ok = xpcall(function() error(\"x\") end, {handler})\n\
                 return {{ type = 0 }}"
            ),
        )]);
        let w = why(header(&d));
        assert!(
            want.iter().any(|l| w == LuaWhy::Limit(*l)),
            "{handler}: {w:?}"
        );
    }
    // A handler that completes returns its value as Lua does.
    let d = folder(&[(
        "s.luaskin",
        "local ok, v = xpcall(function() error(\"x\", 0) end, function(e) return e .. \"!\" end)\n\
         assert(not ok and v == \"x!\")\nreturn { type = 0 }",
    )]);
    header(&d).unwrap();
}

#[test]
fn cancellation_refuses_both_passes() {
    let d = folder(&[("s.luaskin", &drawn(""))]);
    let cancel = Cancel::new();
    cancel.cancel();
    assert_eq!(
        why(read_lua_header(d.path(), "s.luaskin", &cancel)),
        LuaWhy::Cancelled
    );
    let h = header(&d).unwrap();
    assert_eq!(
        why(h.import(&Config::defaults(h.header()), &cancel)),
        LuaWhy::Cancelled
    );
}

#[test]
fn cancellation_stops_a_running_pass() {
    // An endless loop of host calls stops only for the cancellation,
    // which another thread requests once the pass runs.
    let d = folder(&[(
        "s.luaskin",
        "local n = 0\nwhile true do n = n + 1 if n % 100 == 0 then setmetatable({}, nil) end end",
    )]);
    let cancel = Cancel::new();
    let remote = cancel.clone();
    let path = d.path().to_owned();
    let worker = std::thread::spawn(move || read_lua_header(&path, "s.luaskin", &cancel));
    std::thread::sleep(std::time::Duration::from_millis(5));
    remote.cancel();
    let r = worker.join().unwrap();
    // The instruction cap could be reached first on a very fast machine;
    // either is a refusal, and a cancellation is what this one sees.
    let w = why(r);
    assert!(
        w == LuaWhy::Cancelled || w == LuaWhy::Limit(LuaLimit::Instructions),
        "{w:?}"
    );
}

#[test]
fn results_and_errors_cross_threads() {
    fn send<T: Send + 'static>() {}
    send::<LuaHeader>();
    send::<LuaImported>();
    send::<ImportError>();
    let d = folder(&[("s.luaskin", &drawn(""))]);
    let path = d.path().to_owned();
    let h = std::thread::spawn(move || read_lua_header(&path, "s.luaskin", &Cancel::new()))
        .join()
        .unwrap()
        .unwrap();
    let r = std::thread::spawn(move || h.import(&Config::defaults(h.header()), &Cancel::new()))
        .join()
        .unwrap();
    assert_eq!(r.unwrap().imported.report.usage.elements, 1);
}

#[test]
fn loaders_refuse_names_outside_the_skin_folder() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("x.lua"), "return {}").unwrap();
    let abs = outside
        .path()
        .join("x.lua")
        .display()
        .to_string()
        .replace('\\', "/");
    for (call, want) in [
        ("dofile(\"../x.lua\")", RootError::Underflow),
        ("dofile(\"/x.lua\")", RootError::Rejected),
        ("dofile(\"\\\\x.lua\")", RootError::Rejected),
        ("dofile(\"C:/x.lua\")", RootError::Rejected),
        ("dofile(\"//server/share/x.lua\")", RootError::Rejected),
        (
            "dofile(\"\\\\\\\\server\\\\share\\\\x.lua\")",
            RootError::Rejected,
        ),
        ("dofile(\"m.lua:stream\")", RootError::Rejected),
        ("dofile(\"parts/*.lua\")", RootError::Rejected),
        ("pcall(dofile, \"../x.lua\")", RootError::Underflow),
        ("pcall(dofile, \"missing.lua\")", RootError::Missing),
        ("require(\"..x\")", RootError::Rejected),
        ("require(\"a/b\")", RootError::Rejected),
        ("pcall(require, \"c:x\")", RootError::Rejected),
        ("pcall(require, \"missing\")", RootError::Missing),
    ] {
        let d = folder(&[
            ("s.luaskin", &format!("{call}\nreturn {{ type = 0 }}")),
            ("m.lua", "return {}"),
        ]);
        let got = read(header(&d));
        assert_eq!(format!("{got:?}"), format!("{want:?}"), "{call}");
    }
    let d = folder(&[(
        "s.luaskin",
        &format!("dofile(\"{abs}\")\nreturn {{ type = 0 }}"),
    )]);
    assert!(matches!(read(header(&d)), RootError::Rejected));
    // A loader's argument must be a name.
    let d = folder(&[("s.luaskin", "pcall(dofile, 1)\nreturn { type = 0 }")]);
    assert_eq!(why(header(&d)), LuaWhy::LoaderName);
}

#[test]
fn a_link_out_of_the_skin_folder_is_refused() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("x.lua"), "return {}").unwrap();
    let d = folder(&[("s.luaskin", "require(\"link\")\nreturn { type = 0 }")]);
    let target = outside.path().join("x.lua");
    let link = d.path().join("link.lua");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target, &link).unwrap();
    assert!(matches!(read(header(&d)), RootError::Escapes));
}

#[test]
fn require_caches_a_module_and_dofile_runs_each_time() {
    let d = folder(&[
        (
            "s.luaskin",
            &skin(
                "",
                &format!(
                    "local a = require(\"parts.m\")\nlocal b = require(\"parts.m\")\n\
                     assert(a == b and a.runs == 1, \"cached\")\n\
                     dofile(\"parts/d.lua\")\nlocal n = dofile(\"parts/d.lua\")\n\
                     assert(n == 2, \"dofile\")\n{DRAW_I}"
                ),
            ),
        ),
        (
            "parts/m.lua",
            "runs = (runs or 0) + 1\nreturn { runs = runs }",
        ),
        ("parts/d.lua", "ran = (ran or 0) + 1\nreturn ran"),
    ]);
    let r = import(&d).unwrap();
    assert_eq!(r.dependencies, ["s.luaskin", "parts/m.lua", "parts/d.lua"]);
    assert_eq!((r.usage.requires, r.usage.dofiles), (2, 2));
}

#[test]
fn a_require_cycle_is_refused() {
    let d = folder(&[
        ("s.luaskin", "require(\"a\")\nreturn { type = 0 }"),
        ("a.lua", "require(\"b\")\nreturn {}"),
        ("b.lua", "pcall(require, \"a\")\nreturn {}"),
    ]);
    assert_eq!(
        located(header(&d)),
        (LuaWhy::RequireCycle, "b.lua".into(), 1)
    );
}

#[test]
fn a_part_that_fails_is_refused_inside_a_protected_call() {
    let d = folder(&[
        ("s.luaskin", "pcall(dofile, \"p.lua\")\nreturn { type = 0 }"),
        ("p.lua", "local x = 1\nerror(\"broken\")"),
    ]);
    assert_eq!(located(header(&d)), (LuaWhy::Error, "p.lua".into(), 2));
    let d = folder(&[
        ("s.luaskin", "pcall(require, \"p\")\nreturn { type = 0 }"),
        ("p.lua", "local x = \n"),
    ]);
    assert_eq!(located(header(&d)), (LuaWhy::Syntax, "p.lua".into(), 2));
}

#[test]
fn the_loader_depth_is_capped() {
    let chain = |n: u32| {
        let mut files: Vec<(String, String)> = (0..n)
            .map(|i| {
                let next = if i + 1 < n {
                    format!("require(\"m{}\")\n", i + 1)
                } else {
                    String::new()
                };
                (format!("m{i}.lua"), format!("{next}return {{}}"))
            })
            .collect();
        files.push((
            "s.luaskin".into(),
            "require(\"m0\")\nreturn { type = 0 }".into(),
        ));
        files
    };
    let at = chain(MAX_INCLUDE_DEPTH);
    let refs: Vec<(&str, &str)> = at.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    header(&folder(&refs)).unwrap();
    let over = chain(MAX_INCLUDE_DEPTH + 1);
    let refs: Vec<(&str, &str)> = over.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    assert_eq!(input(header(&folder(&refs))), InputLimit::IncludeDepth);
}

#[test]
fn every_load_invocation_counts() {
    let n = MAX_INCLUDE_PARSES;
    let loads = |n: u32| {
        folder(&[
            (
                "s.luaskin",
                &skin(
                    "",
                    &format!("for i = 1, {n} do dofile(\"p.lua\") end\n{DRAW_I}"),
                ),
            ),
            ("p.lua", "return 1"),
        ])
    };
    let r = import(&loads(n)).unwrap();
    assert_eq!(r.usage.loads, n);
    assert_eq!(input(import(&loads(n + 1))), InputLimit::IncludeParses);
    // A cached `require` counts too.
    let d = folder(&[
        (
            "s.luaskin",
            &skin(
                "",
                &format!("for i = 1, {} do require(\"m\") end\n{DRAW_I}", n + 1),
            ),
        ),
        ("m.lua", "return {}"),
    ]);
    assert_eq!(input(import(&d)), InputLimit::IncludeParses);
}

#[test]
fn source_bytes_are_capped_per_file_and_in_all() {
    let big = format!(
        "--{}\nreturn 1",
        "x".repeat(usize::try_from(MAX_FILE_BYTES).unwrap())
    );
    let d = folder(&[
        ("s.luaskin", "dofile(\"big.lua\")\nreturn { type = 0 }"),
        ("big.lua", &big),
    ]);
    assert_eq!(input(header(&d)), InputLimit::FileBytes);
    // A 1 MiB part read again and again: each read counts.
    let part = format!("--{}\nreturn 1", "x".repeat((1 << 20) - 12));
    let reads = |n: u32| {
        folder(&[
            (
                "s.luaskin",
                &skin(
                    "",
                    &format!("for i = 1, {n} do dofile(\"p.lua\") end\n{DRAW_I}"),
                ),
            ),
            ("p.lua", &part),
        ])
    };
    let r = import(&reads(31)).unwrap();
    assert!(r.usage.input_bytes < 32 << 20, "{}", r.usage.input_bytes);
    assert_eq!(input(import(&reads(32))), InputLimit::InputBytes);
}

#[test]
fn the_passes_diverge_on_skin_config_and_share_nothing() {
    let d = folder(&[
        (
            "s.luaskin",
            &skin(
                "property = { { name = \"opt\", item = { { name = \"a\", op = 900 }, { name = \"b\", op = 901 } }, def = \"b\" } }",
                &format!(
                    "assert(leaked == nil, \"a global leaked\")\n\
                     assert(require(\"m\").runs == 1, \"a module leaked\")\n\
                     assert(skin_config.option.opt == 901, \"option\")\n{DRAW_I}"
                ),
            )
            .replace(
                "if not skin_config then return skin end",
                "if not skin_config then leaked = true require(\"m\") return skin end",
            ),
        ),
        ("m.lua", "runs = (runs or 0) + 1\nreturn { runs = runs }"),
    ]);
    let h = header(&d).unwrap();
    assert_eq!(h.header().options.len(), 1);
    // The header pass ran without `skin_config`; the configured pass saw
    // the default item, then the configured one.
    h.import(&Config::defaults(h.header()), &Cancel::new())
        .unwrap();
    let other = Config {
        options: vec![Some(900)],
        ..Config::default()
    };
    let (w, _, line) = located(h.import(&other, &Cancel::new()));
    assert_eq!((w, line), (LuaWhy::Error, 7));
}

#[test]
fn skin_config_follows_the_header_and_its_writes_stay_in_the_pass() {
    let d = folder(&[
        (
            "s.luaskin",
            &skin(
                "property = { { name = \"opt\", item = { { name = \"a\", op = 900 }, { name = \"b\", op = 901 } }, def = \"b\" } },\n\
                 filepath = { { name = \"bg\", path = \"bg/*.png\", def = \"b\" } },\n\
                 offset = { { name = \"o\", id = 40, x = true } },",
                &format!(
                    "assert(skin_config.file_path.bg == \"bg/b.png\", \"file_path\")\n\
                     assert(skin_config.get_path(\"bg/*.png\") == \"bg/b.png\", \"get_path\")\n\
                     assert(skin_config.get_path(\"bg/*.png/x\") == \"bg/b.png/x\", \"get_path foot\")\n\
                     assert(skin_config.get_path(\"fx/*.png\") == nil, \"unchosen\")\n\
                     assert(skin_config.option.opt == 901, \"option\")\n\
                     assert(#skin_config.enabled_options == 1 and skin_config.enabled_options[1] == 901, \"enabled\")\n\
                     assert(skin_config.offset.o.x == 5 and skin_config.offset.o.y == 0, \"offset at start\")\n\
                     local n = 0 for _ in pairs(skin_config.offset.o) do n = n + 1 end\n\
                     assert(n == 6, \"pairs\")\n\
                     -- The skin's own writes are seen by its later reads.\n\
                     skin_config.offset.o.x = 123\n\
                     skin_config.option.opt = 900\n\
                     rawset(skin_config, \"extra\", true)\n\
                     assert(skin_config.offset.o.x == 123 and skin_config.option.opt == 900, \"written\")\n\
                     assert(skin_config.extra == true, \"rawset\")\n{DRAW_I}"
                ),
            ),
        ),
        ("bg/a.png", ""),
        ("bg/b.png", ""),
    ]);
    let h = header(&d).unwrap();
    let mut config = Config::defaults(h.header());
    config.offsets.insert(
        40,
        OffsetValue {
            x: 5,
            ..OffsetValue::default()
        },
    );
    let kept = config.clone();
    // Each configured pass starts from the host's values again: the
    // second would fail its first assertions after the first's writes.
    h.import(&config, &Cancel::new()).unwrap();
    h.import(&config, &Cancel::new()).unwrap();
    assert_eq!(config, kept);
}

#[test]
fn host_modules_hold_the_pinned_names_and_refuse_calls() {
    // The header pass's modules are empty, as the source loads them.
    let d = folder(&[(
        "s.luaskin",
        "if not skin_config then\n\
           for _, m in ipairs({ \"main_state\", \"timer_util\", \"event_util\" }) do\n\
             assert(next(require(m)) == nil, m)\n\
           end\n\
           return { type = 0 }\n\
         end\n\
         local ms = require(\"main_state\")\n\
         assert(ms.timer_off_value == math.mininteger, \"off value\")\n\
         assert(ms.event_observe_boolean == nil and ms.lua_version == nil, \"absent\")\n\
         assert(require(\"event_util\").event_observe_boolean == nil, \"absent event\")\n\
         return { type = 0 }",
    )]);
    let h = header(&d).unwrap();
    // Each pinned name is a function in its module, and calling it is
    // refused as reading play state.
    for (index, &(module, api)) in HOST_APIS.iter().enumerate() {
        let d = folder(&[(
            "s.luaskin",
            &format!(
                "if not skin_config then return {{ type = 0 }} end\n\
                 local f = require(\"{module}\").{api}\n\
                 assert(type(f) == \"function\", \"{api}\")\n\
                 pcall(f, 1)\n\
                 return {{ type = 0 }}"
            ),
        )]);
        let h = header(&d).unwrap();
        let w = why(h.import(&Config::defaults(h.header()), &Cancel::new()));
        assert_eq!(
            w,
            LuaWhy::LiveState {
                api: u16::try_from(index).unwrap()
            },
            "{module}.{api}"
        );
    }
    h.import(&Config::defaults(h.header()), &Cancel::new())
        .unwrap();
}

#[test]
fn every_function_in_the_definition_is_refused_at_its_definition() {
    // A decorative image's condition and timer are refused, not frozen.
    for (fields, field, line) in [
        ("draw = function() return true end,", "draw", 5),
        ("timer = function() return 0 end,", "timer", 5),
        ("op = { function() end },", "other", 5),
    ] {
        let d = folder(&[("s.luaskin", &drawn(fields))]);
        assert_eq!(
            located(import(&d)),
            (LuaWhy::Callable { field }, "s.luaskin".into(), line),
            "{fields}"
        );
    }
    // A gauge's condition.
    let images: String = (0..4)
        .map(|k| {
            format!(
                "{{ id = \"n{k}\", src = \"a\", x = {}, y = 0, w = 8, h = 8 }},",
                8 * k
            )
        })
        .collect();
    let d = folder(&[(
        "s.luaskin",
        &skin(
            "",
            &format!(
                "skin.image = {{ {images} }}\n\
                 skin.gauge = {{ id = \"g\", nodes = {{ \"n0\", \"n1\", \"n2\", \"n3\" }}, parts = 10 }}\n\
                 skin.destination = {{ {{ id = \"g\",\n\
                   draw = function() return true end,\n\
                   dst = {{ {{ x = 100, y = 500, w = 100, h = 10 }} }} }} }}"
            ),
        ),
    )]);
    assert_eq!(
        located(import(&d)),
        (LuaWhy::Callable { field: "draw" }, "s.luaskin".into(), 8)
    );
    // A number's value, a field that takes no function, and a field
    // nothing reads.
    let d = folder(&[(
        "s.luaskin",
        &skin(
            "",
            "skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 3,\n\
               value = function() return 1 end } }\n\
             skin.destination = { { id = \"v\", dst = { { w = 6, h = 6 } } } }",
        ),
    )]);
    assert_eq!(
        located(import(&d)),
        (LuaWhy::Callable { field: "value" }, "s.luaskin".into(), 6)
    );
    let d = folder(&[("s.luaskin", &drawn("blend = function() end,"))]);
    assert_eq!(why(import(&d)), LuaWhy::Callable { field: "blend" });
    let d = configured("skin.unread = { function() end }");
    assert_eq!(why(import(&d)), LuaWhy::Callable { field: "other" });
    // A module's function is located in that module.
    let d = folder(&[
        ("s.luaskin", &drawn("loop = require(\"f\"),")),
        ("f.lua", "local x = 1\nreturn function() end"),
    ]);
    assert_eq!(
        located(import(&d)),
        (LuaWhy::Callable { field: "loop" }, "f.lua".into(), 2)
    );
    // A host function has no Lua definition: it is located at its value,
    // line 0 and its ordinal in the table.
    let d = configured("skin.timer_fn = require(\"timer_util\").now_timer");
    match import(&d) {
        Err(ImportError::Lua {
            why: LuaWhy::Callable { field: "other" },
            at: Some(at),
        }) => assert_eq!((at.file.as_str(), at.line), ("s.luaskin", 0)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_header_pass_refuses_a_function_only_where_it_reads() {
    // Where it does not: a key of an option item the header never reads,
    // or a field outside the header. The header pass reads past it; the
    // configured pass refuses it at its definition.
    for fields in [
        "property = { { name = \"o\", item = { { name = \"a\", op = 900,\n is_selected = function() end } } } }",
        "w = 1920, h = 1080,\n helper = function() end",
    ] {
        let d = folder(&[("s.luaskin", &format!("return {{ type = 0, {fields} }}"))]);
        let h = header(&d).unwrap_or_else(|e| panic!("{fields}: {e:?}"));
        let (w, file, line) = located(h.import(&Config::defaults(h.header()), &Cancel::new()));
        assert!(matches!(w, LuaWhy::Callable { .. }), "{fields}: {w:?}");
        assert_eq!((file.as_str(), line), ("s.luaskin", 2), "{fields}");
    }
    // Where the header's reading meets it: a value, a record, a list or a
    // condition, refused by the header pass at its definition.
    for (fields, field, line) in [
        (
            "property = { { name = \"o\",\n def = function() end } }",
            "def",
            2,
        ),
        ("property = {\n function() end }", "other", 2),
        ("filepath =\n function() end", "other", 2),
        (
            "offset = { { name = \"o\",\n id = function() end } }",
            "id",
            2,
        ),
        (
            "property = { { [\"if\"] =\n function() end, value = {} } }",
            "if",
            2,
        ),
        // Every operand a condition visits, and every choice branch.
        (
            "property = { { [\"if\"] = {\n function() end }, value = {} } }",
            "if",
            2,
        ),
        (
            "property = { { [\"if\"] = { {\n function() end } }, value = {} } }",
            "if",
            2,
        ),
        ("property = { {\n function() end } }", "other", 2),
        // A function as an option's item list, or as one item.
        (
            "property = { { name = \"o\", item =\n function() end } }",
            "item",
            2,
        ),
        (
            "property = { { name = \"o\", item = {\n function() end } } }",
            "item",
            2,
        ),
    ] {
        let d = folder(&[("s.luaskin", &format!("return {{ type = 0, {fields} }}"))]);
        assert_eq!(
            located(header(&d)),
            (LuaWhy::Callable { field }, "s.luaskin".into(), line),
            "{fields}"
        );
    }
    // A condition stops at its first operand that fails, as it always
    // has: the function after it is not visited.
    let d = folder(&[(
        "s.luaskin",
        "return { type = 0, property = { { [\"if\"] = { 901, function() end }, value = { name = \"o\" } } } }",
    )]);
    assert!(header(&d).unwrap().header().options.is_empty());
}

#[test]
fn json_included_by_a_lua_skin_is_charged_to_the_candidate() {
    // The header's options come from a JSON file, read in both passes.
    let pad = |n: usize| format!("[{}{{\"name\": \"o\"}}]", " ".repeat(n));
    let header_json = pad(15 << 20);
    let entry = skin(
        "property = { include = \"props.json\" },",
        &format!("skin.image = {{ include = \"images.json\" }}\n{DRAW_I}"),
    );
    let images = |n: usize| {
        format!(
            "[{}{{\"id\": \"i\", \"src\": \"a\", \"w\": 64, \"h\": 64}}]",
            " ".repeat(n)
        )
    };
    let small = folder(&[
        ("s.luaskin", &entry),
        ("props.json", &pad(0)),
        ("images.json", &images(0)),
    ]);
    let r = import(&small).unwrap();
    assert_eq!(r.dependencies, ["s.luaskin", "props.json", "images.json"]);
    assert_eq!(
        r.imported.report.sources,
        ["s.luaskin", "props.json", "images.json"]
    );
    // 15 MiB in each pass and 3 MiB more pass the 32 MiB in all.
    let big = folder(&[
        ("s.luaskin", &entry),
        ("props.json", &header_json),
        ("images.json", &images(3 << 20)),
    ]);
    let h = header(&big).unwrap();
    assert!(h.usage().input_bytes >= 15 << 20, "{:?}", h.usage());
    assert_eq!(
        input(h.import(&Config::defaults(h.header()), &Cancel::new())),
        InputLimit::InputBytes
    );
}

#[test]
fn json_included_by_a_lua_skin_shares_the_load_allowance() {
    // 600 includes in each pass: 1,200 in all, over the 1,024 the
    // candidate's loads and includes share.
    let includes = "{ include = \"p.json\" },".repeat(600);
    let d = folder(&[
        (
            "s.luaskin",
            &format!("return {{ type = 0, property = {{ {includes} }} }}"),
        ),
        ("p.json", "[]"),
    ]);
    let h = header(&d).unwrap();
    assert_eq!(h.usage().loads, 600);
    assert_eq!(
        input(h.import(&Config::defaults(h.header()), &Cancel::new())),
        InputLimit::IncludeParses
    );
}

#[test]
fn json_included_by_a_lua_skin_is_read_once_for_both_passes() {
    // The configured pass reads the header pass's snapshot: a file changed
    // between the passes is not read again.
    let d = folder(&[
        (
            "s.luaskin",
            "return { type = 0, property = { include = \"p.json\" } }",
        ),
        ("p.json", "[{\"name\": \"o\"}]"),
    ]);
    let h = header(&d).unwrap();
    std::fs::write(d.path().join("p.json"), "this is no JSON").unwrap();
    h.import(&Config::defaults(h.header()), &Cancel::new())
        .unwrap();
}

#[test]
fn numbers_keep_their_subtype_and_exact_value() {
    // A whole float is an integer; a fraction is not one for exact
    // geometry; an integer outside i32 is out of range, not wrapped; a
    // NaN has no integer.
    let ok = folder(&[("s.luaskin", &drawn("blend = 2.0, filter = 1,"))]);
    import(&ok).unwrap();
    // A digit offset is read exactly.
    let offsets = |x: &str| {
        skin(
            "",
            &format!(
                "skin.value = {{ {{ id = \"v\", src = \"a\", w = 60, h = 6, divx = 10, digit = 1,\n\
                   ref = 110, offset = {{ {{ x = {x} }} }} }} }}\n{DRAW_I}"
            ),
        )
    };
    import(&folder(&[("s.luaskin", &offsets("2.0"))])).unwrap();
    let r = import(&folder(&[("s.luaskin", &offsets("1.5"))]));
    assert!(
        matches!(r, Err(ImportError::Type { field: "x", .. })),
        "{r:?}"
    );
    for value in ["4294967297", "-2147483649", "0/0", "1/0", "math.mininteger"] {
        let d = folder(&[("s.luaskin", &drawn(&format!("loop = {value},")))]);
        let r = import(&d);
        assert!(
            matches!(r, Err(ImportError::Range { field: "loop", .. })),
            "{value}: {r:?}"
        );
    }
    // An integer names a source by its digits; a float names none.
    let by_int = skin(
        "",
        "skin.source = { { id = 7, path = \"a.png\" } }\n\
         skin.image = { { id = \"i\", src = 7, x = 0, y = 0, w = 64, h = 64 } }\n{DRAW_I}"
            .replace("{DRAW_I}", DRAW_I)
            .as_str(),
    );
    let r = import(&folder(&[("s.luaskin", &by_int)])).unwrap();
    assert_eq!(r.imported.report.usage.elements, 1);
    assert!(
        r.imported.report.notes.is_empty(),
        "{:?}",
        r.imported.report.notes
    );
    let by_float = by_int.replace("src = 7,", "src = 7.0,");
    let r = import(&folder(&[("s.luaskin", &by_float)]));
    assert!(
        matches!(r, Err(ImportError::Type { field: "src", .. })),
        "{r:?}"
    );
}

#[test]
fn tables_are_read_raw_and_their_shape_is_checked() {
    // Metamethods are never invoked.
    let guarded = configured(
        "setmetatable(skin, { __index = function() error(\"index\") end,\n\
           __pairs = function() error(\"pairs\") end, __len = function() error(\"len\") end })",
    );
    import(&guarded).unwrap();
    for (body, want) in [
        ("skin.extra = { 1, 2, x = 3 }", LuaWhy::Mixed),
        ("skin.extra = { 1, nil, 3 }", LuaWhy::Sparse),
        ("skin.extra = { [0] = 1 }", LuaWhy::Sparse),
        ("skin.extra = { [1.5] = 1 }", LuaWhy::Key),
        ("skin.extra = { [true] = 1 }", LuaWhy::Key),
        ("skin.extra = skin", LuaWhy::Cycle),
        ("skin.extra = \"\\xff\"", LuaWhy::NotUtf8),
    ] {
        assert_eq!(why(import(&configured(body))), want, "{body}");
    }
    let d = folder(&[("s.luaskin", "return 1")]);
    assert_eq!(why(header(&d)), LuaWhy::Returned);
    // A table reached twice is converted twice; an empty one takes its
    // place's kind: an empty `dst` is no keys, an empty `mouseRect` an
    // object (the element is dropped).
    let shared = folder(&[(
        "s.luaskin",
        &skin(
            "",
            "local d = { { x = 0, y = 0, w = 64, h = 64 } }\n\
             skin.destination = { { id = \"i\", dst = d }, { id = \"i\", dst = d }, { id = \"i\", dst = {} } }",
        ),
    )]);
    let r = import(&shared).unwrap();
    assert_eq!(r.imported.report.usage.elements, 3);
    // An empty condition is an empty list of conditions whatever the
    // wrapper's case: it holds, as JSON's `[]` does, so every record is
    // drawn.
    let conditions = folder(&[(
        "s.luaskin",
        &skin(
            "",
            "local d = { { x = 0, y = 0, w = 64, h = 64 } }\n\
             skin.destination = {\n\
               { [\"if\"] = {}, value = { id = \"i\", dst = d } },\n\
               { IF = {}, value = { id = \"i\", dst = d } },\n\
               { If = {}, values = { { id = \"i\", dst = d } } },\n\
             }",
        ),
    )]);
    let r = import(&conditions).unwrap();
    assert_eq!(r.imported.report.usage.elements, 3);
    assert!(
        r.imported.report.notes.is_empty(),
        "{:?}",
        r.imported.report.notes
    );
    let mouse = folder(&[(
        "s.luaskin",
        &skin(
            "",
            "skin.destination = { { id = \"i\", mouseRect = {}, dst = { { w = 64, h = 64 } } } }",
        ),
    )]);
    let r = import(&mouse).unwrap();
    assert_eq!(
        r.imported.report.notes[0].kind,
        NoteKind::UnsupportedField { field: "mouseRect" }
    );
}

#[test]
fn the_converted_table_takes_the_definition_bounds() {
    // A few lines that build 8^7 values: refused at the node bound before
    // they are all kept.
    let d = configured(
        "local t = { 1, 2, 3, 4, 5, 6, 7, 8 }\n\
         for i = 1, 6 do t = { t, t, t, t, t, t, t, t } end\nskin.extra = t",
    );
    assert_eq!(input(import(&d)), InputLimit::Nodes);
    assert!(8_usize.pow(7) + 8_usize.pow(6) > MAX_NODES);
    let deep = configured("local t = {}\nfor i = 1, 70 do t = { t } end\nskin.extra = t");
    assert_eq!(input(import(&deep)), InputLimit::Depth);
    let long = configured("skin.extra = string.rep(\"x\", (64 << 10) + 1)");
    assert_eq!(input(import(&long)), InputLimit::StringBytes);
    let many = configured(
        "local s = string.rep(\"x\", 64 << 10)\nlocal t = {}\n\
         for i = 1, 520 do t[i] = s end\nskin.extra = t",
    );
    assert_eq!(input(import(&many)), InputLimit::DecodedBytes);
}

#[test]
fn errors_are_located_in_their_module() {
    let d = folder(&[
        ("s.luaskin", "require(\"m\")\nreturn { type = 0 }"),
        ("m.lua", "\n\nlocal t = nil\nreturn t.x"),
    ]);
    assert_eq!(located(header(&d)), (LuaWhy::Error, "m.lua".into(), 4));
    let d = folder(&[("s.luaskin", "return {\n type = 0,\n w = = 1 }")]);
    assert_eq!(located(header(&d)), (LuaWhy::Syntax, "s.luaskin".into(), 3));
    let d = folder(&[("s.luaskin", "\x1bLua")]);
    assert_eq!(why(header(&d)), LuaWhy::Syntax);
}
