//! Lua skins through the public entries: every test runs
//! [`read_lua_header`] (the header pass) and, where it imports,
//! [`LuaHeader::import`] (the configured pass, the conversion and the
//! common import). Every fixture is a small self-made skin.

mod common;

use skin_beatoraja::limits::{MAX_FILE_BYTES, MAX_INCLUDE_DEPTH, MAX_INCLUDE_PARSES, MAX_NODES};
use skin_beatoraja::lua::limits::{MAX_HOST_CALLS, MAX_INSTRUCTIONS, MAX_LUA_TIME};
use skin_beatoraja::lua::{Disposition, HOST_APIS};
use skin_beatoraja::{
    Cancel, Config, ImportError, InputLimit, LuaHeader, LuaImported, LuaLimit, LuaWhy, NoteKind,
    OffsetValue, Severity, read_lua_header,
};
use skin_model::{RootError, ScreenKind};
use tempfile::TempDir;

/// A `rep` the fixtures build long strings with, by the VM's own
/// concatenation (the library has no `string.rep`), on the entry's first
/// line so that its line numbers stay.
const REP: &str = "local function rep(s, n) local r, p = \"\", s \
    while n > 0 do if n % 2 == 1 then r = r .. p end n = n // 2 \
    if n > 0 then p = p .. p end end return r end ";

/// A skin folder with `files` and a 64×64 `a.png`; the entry
/// `s.luaskin` has [`REP`].
fn folder(files: &[(&str, &str)]) -> TempDir {
    let d = tempfile::tempdir().unwrap();
    for &(name, text) in files {
        if name == "s.luaskin" {
            common::write(d.path(), name, format!("{REP}{text}").as_bytes());
        } else {
            common::write(d.path(), name, text.as_bytes());
        }
    }
    common::png(d.path(), "a.png", 64, 64);
    d
}

/// The header pass of `s.luaskin`.
fn header(d: &TempDir) -> Result<LuaHeader, ImportError> {
    read_lua_header(d.path(), "s.luaskin", &Cancel::new())
}

/// Both passes of `s.luaskin` with the default configuration.
fn import(d: &TempDir) -> Result<LuaImported, ImportError> {
    common::lua_import(d.path())
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
        "string.rep",
        "string.reverse",
        "string.gmatch",
        "table.move",
        "table.unpack",
        "table.pack",
        "rawequal",
        "rawget",
        "rawlen",
        "rawset",
        "math.random",
        "math.randomseed",
        "math.tointeger",
        "math.type",
        "math.mininteger",
        "math.huge",
    ];
    let present = [
        "assert",
        "error",
        "getmetatable",
        "ipairs",
        "next",
        "pairs",
        "pcall",
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
        "string.len",
        "string.sub",
        "string.byte",
        "string.char",
        "string.upper",
        "string.lower",
        "string.match",
        "string.gsub",
        "table.insert",
        "table.remove",
        "math.floor",
        "math.ceil",
        "math.abs",
        "math.max",
        "math.min",
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
        why(common::lua_play(
            &h,
            &Config::defaults(h.header()),
            &Cancel::new()
        )),
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
        "local s = rep(\"x\", 1 << 20)\nlocal t = {}\nfor i = 1, 100 do t[i] = s .. i end",
        "pcall(function() local s = \"x\" for i = 1, 40 do s = s .. s end end)",
        // Inside a primitive: the argument is built once, the copies made
        // by `upper` itself.
        "local s = rep(\"x\", 1 << 19)\nlocal t = {}\n\
         pcall(function() for i = 1, 160 do t[i] = string.upper(s) end end)",
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
         assert(string.find(\"a+b\", \"+\", 1, true) == 2)\n\
         assert(string.format(\"%02d:%s\", 7, \"x\") == \"07:x\")\n\
         assert(tonumber(\"0x10\") == 16 and tonumber(\" 42 \", 10) == 42 and tonumber(\"4x\", 10) == nil)\n\
         local t = { 3, 1, 2 }\n\
         table.sort(t) assert(t[1] == 1 and t[3] == 3)\n\
         table.sort(t, function(a, b) return a > b end) assert(t[1] == 3)\n\
         table.insert(t, 1, 9) assert(t[1] == 9 and #t == 4)\n\
         assert(table.remove(t) == 1 and #t == 3)\n\
         assert(table.concat({ 1, \"b\", 3 }, \",\") == \"1,b,3\")\n\
         assert(math.max(1, 3, 2) == 3 and math.min(2.5, 1) == 1 and math.floor(2.5) == 2)\n\
         assert((\"abc\"):upper() == \"ABC\" and (\"abc\"):sub(2) == \"bc\")",
    );
    import(&fine).unwrap();
    // Backtracking and long copies are instructions: the cap ends them.
    for body in [
        "string.find(rep(\"a\", 3000), \"a*a*a*b\")",
        "pcall(string.gsub, rep(\"a\", 3000), \"(a-)(a-)(a-)b\", \"\")",
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
    // the call; small results add up to the byte budget; an unsupported
    // form is refused, never approximated.
    let cap = LuaWhy::Library {
        op: "result over the primitive cap",
    };
    let format = LuaWhy::Library {
        op: "format conversion",
    };
    let tonumber = LuaWhy::Library {
        op: "tonumber of a long numeral or another base",
    };
    let zero = LuaWhy::Library { op: "pattern %z" };
    // Varargs of `n` copies of 65, for a call with that many arguments.
    let args = "local function many(k, ...) if k == 0 then return ... end return many(k - 1, 65, ...) end\n";
    for (body, want) in [
        (
            "local s = rep(\"a\", 1 << 20)\npcall(string.sub, s .. \"b\", 1)".to_owned(),
            cap,
        ),
        (
            "local s = rep(\"a\", 1 << 20)\nstring.format(\"%s%s\", s, s)".to_owned(),
            cap,
        ),
        (
            "pcall(string.byte, rep(\"a\", 5000), 1, -1)".to_owned(),
            cap,
        ),
        (format!("{args}pcall(string.char, many(5000))"), cap),
        (
            "local s = rep(\"a\", 1 << 19)\nfor i = 1, 300 do local t = s:sub(1) end".to_owned(),
            LuaWhy::Limit(LuaLimit::NativeWork),
        ),
        (
            "local s = rep(\"a\", 1 << 19)\nfor i = 1, 300 do local t = s:upper() end".to_owned(),
            LuaWhy::Limit(LuaLimit::NativeWork),
        ),
        // A metatable the table functions would consult is refused, so
        // `__len` is never read twice.
        (
            "local k = 0\n\
             local t = setmetatable({}, { __len = function() k = k + 1 return k == 1 and 0 or 1 << 40 end })\n\
             table.insert(t, 1, false)"
                .to_owned(),
            LuaWhy::Library {
                op: "table with a metatable",
            },
        ),
        (
            "pcall(table.concat, setmetatable({}, { __index = {} }))".to_owned(),
            LuaWhy::Library {
                op: "table with a metatable",
            },
        ),
        (
            "string.find(\"(x)\", \"%b()\")".to_owned(),
            LuaWhy::Library { op: "pattern %b" },
        ),
        (
            "pcall(string.match, \"aa\", \"(a)%1\")".to_owned(),
            LuaWhy::Library {
                op: "pattern back reference",
            },
        ),
        // `%z` is refused, not read as a literal `z`, in every API and
        // inside a set.
        ("pcall(string.find, \"z\", \"%z\")".to_owned(), zero),
        ("pcall(string.match, \"z\", \"%Z\")".to_owned(), zero),
        ("pcall(string.gsub, \"z\", \"[%z]\", \"\")".to_owned(), zero),
        // A conversion the library does not size is refused: `%q`, a
        // width or precision of three digits, one that would wrap.
        ("pcall(string.format, \"%q\", \"\\127\")".to_owned(), format),
        ("pcall(string.format, \"%999d\", 1)".to_owned(), format),
        ("pcall(string.format, \"%.100f\", 1)".to_owned(), format),
        (
            "local s = rep(\"a\", 1 << 19)\n\
             pcall(string.format, \"%s%18446744073692774400d\", s, 0)"
                .to_owned(),
            format,
        ),
        ("pcall(string.format, \"%x\", 1)".to_owned(), format),
        // `tonumber` reads base 10 and short numerals only.
        ("pcall(tonumber, \"10\", 2)".to_owned(), tonumber),
        ("pcall(tonumber, \"10\", 10.0)".to_owned(), tonumber),
        (
            "local s = rep(\"1\", 1 << 20)\npcall(tonumber, s)".to_owned(),
            tonumber,
        ),
    ] {
        let d = configured(&body);
        assert_eq!(why(import(&d)), want, "{body}");
    }
    // A replacement or a join that fails copies nothing: the result is
    // built only once every piece is checked.
    for body in [
        "local big = rep(\"b\", 1 << 19)\n\
         for i = 1, 300 do assert(not pcall(string.gsub, \"ab\", \".\", { a = big, b = {} })) end",
        "local big = rep(\"b\", 1 << 19)\n\
         for i = 1, 300 do assert(not pcall(table.concat, { big, big, {} })) end",
    ] {
        let r = import(&configured(body)).unwrap_or_else(|e| panic!("{body}: {e:?}"));
        assert!(r.usage.native_work < 1 << 20, "{body}: {:?}", r.usage);
    }
    // The natives stay private: the strings' methods are the library's,
    // and their metatable is out of reach.
    let d = configured(
        "assert((\"abc\").upper == string.upper, \"methods\")\n\
         assert(string.dump == nil and (\"\").dump == nil, \"dump\")\n\
         assert(getmetatable(\"\") == false, \"metatable\")\n\
         assert(not pcall(function() getmetatable(\"\").__tostring = function() return \"y\" end end))\n\
         assert(not pcall(setmetatable, \"\", {}))\n\
         assert(string.format(\"%s\", \"x\") == \"x\" and tostring(\"x\") == \"x\")",
    );
    import(&d).unwrap();
}

#[test]
fn the_library_never_coerces_a_string_to_a_number_or_back() {
    // Each is an ordinary error the skin can catch: a string where a
    // number belongs, a number where a string belongs, a fraction where
    // an integer belongs, a value out of range, string arithmetic.
    for call in [
        "string.char(\"65\")",
        "string.char(65.5)",
        "string.char(256)",
        "string.char(-1)",
        "string.sub(\"abc\", \"1\")",
        "string.sub(\"abc\", 1.5)",
        "string.byte(\"abc\", \"1\")",
        "string.upper(1)",
        "string.len(1)",
        "string.find(1, \"1\")",
        "string.find(\"abc\", \"b\", \"1\")",
        "string.gsub(\"abc\", \"b\", \"x\", \"1\")",
        "string.gsub(\"abc\", \"b\", 7)",
        "string.format(\"%d\", \"1\")",
        "string.format(\"%d\", 1.5)",
        "string.format(\"%f\", \"1\")",
        "string.format(1)",
        "math.floor(\"1\")",
        "math.ceil(\"1\")",
        "math.abs(\"1\")",
        "math.max(1, \"2\")",
        "math.min(\"1\")",
        "math.max()",
        "table.insert({}, \"1\", 2)",
        "table.remove({ 1 }, \"1\")",
        "table.concat({ 1 }, 1)",
        "select(\"1\", 1)",
        "error(\"x\", \"1\")",
        "(\"1\") + 1",
        "-(\"1\")",
        "(\"1\") * \"2\"",
        "skin_config.get_path(1)",
    ] {
        let d = configured(&format!(
            "local ok, e = pcall(function() return {call} end)\n\
             assert(not ok and type(e) == \"string\", \"accepted\")"
        ));
        import(&d).unwrap_or_else(|e| panic!("{call}: {e:?}"));
    }
    // What they do take, they take as Lua does.
    let d = configured(
        "assert(string.char(72, 105) == \"Hi\" and string.char() == \"\")\n\
         assert(select(\"#\", string.byte(\"abc\", 1, -1)) == 3 and string.byte(\"abc\", 2.0) == 98)\n\
         assert(string.byte(\"abc\", 10) == nil and string.sub(\"abc\", -2) == \"bc\")\n\
         assert(string.format(\"%5.1f|%-3d|%03d|%.2s|%s|%%\", 2.25, 7, 7, \"abc\", true) == \"  2.2|7  |007|ab|true|%\")\n\
         assert(tonumber(\"12\") == 12 and tonumber(\"1e2\") == 100.0 and tonumber(\" 0x1p4 \") == 16.0)\n\
         assert(tonumber(\"x\") == nil and tonumber({}) == nil and tonumber(\"-7\", 10) == -7)\n\
         assert(math.max(2, 7.5, 7) == 7.5 and math.min(3, -1) == -1 and math.abs(-2) == 2)\n\
         assert(math.floor(-1.5) == -2 and math.ceil(1.2) == 2 and math.floor(3) == 3)\n\
         assert(not pcall(function() return (\"1\") + 1 end))\n\
         assert(skin_config.get_path(\"none.png\") == \"none.png\")",
    );
    import(&d).unwrap();
}

/// Asserts that each of `cases` gives the same results in the sandbox as
/// in an unrestricted VM, the oracle, each serialized the same way.
#[track_caller]
fn agrees_with_lua(cases: &[&str]) {
    const SER: &str = "local function ser(...)\n\
           local n, out = select(\"#\", ...), {}\n\
           for i = 1, n do local v = select(i, ...) out[i] = type(v) .. \":\" .. tostring(v) end\n\
           return table.concat(out, \",\")\n\
         end\n";
    let oracle = mlua::Lua::new();
    for case in cases {
        let want: String = oracle
            .load(format!("{SER}return string.format(\"%q\", ser({case}))"))
            .eval()
            .unwrap_or_else(|e| panic!("{case}: {e}"));
        let d = configured(&format!(
            "{SER}assert(ser({case}) == {want}, \"disagrees\")"
        ));
        import(&d).unwrap_or_else(|e| panic!("{case}: {e:?}"));
    }
}

#[test]
fn the_library_s_matcher_agrees_with_lua_s_own() {
    agrees_with_lua(&[
        "string.find(\"hello world\", \"o w\")",
        "string.find(\"hello\", \"l+\")",
        "string.find(\"hello\", \"(l)(l)\")",
        "string.find(\"hello\", \"()ll()\")",
        "string.find(\"a.b\", \".\", 1, true)",
        "string.find(\"a.b\", \"%.\")",
        "string.find(\"abc\", \"b\", -1)",
        "string.find(\"abc\", \"b\", -10)",
        "string.find(\"abc\", \"\", 10)",
        "string.find(\"abc\", \"\", 4)",
        "string.find(\"abc\", \"^b\")",
        "string.find(\"abc\", \"c$\")",
        "string.find(\"a$c\", \"$c\")",
        "string.match(\"key = value\", \"^(%w+)%s*=%s*(%w+)$\")",
        "string.match(\"  x  \", \"^%s*(.-)%s*$\")",
        "string.match(\"2024-01-02\", \"(%d+)-(%d+)-(%d+)\")",
        "string.match(\"abc123\", \"%a*\")",
        "string.match(\"abc123\", \"[%a_][%w_]*\")",
        "string.match(\"x-y\", \"[a-x]%-[^a-x]\")",
        "string.match(\"]\", \"[]]\")",
        "string.match(\"a^\", \"[%^^]\")",
        "string.match(\"tab\\there\", \"%c\")",
        "string.match(\"AbC\", \"%u%l%u\")",
        "string.match(\"f0A\", \"%x+\")",
        "string.match(\"a,b;\", \"%p\")",
        "string.match(\"a b\", \"%S+%s%S+\")",
        "string.match(\"aaa\", \"a-b\")",
        "string.match(\"aaab\", \"a-b\")",
        "string.match(\"ab\", \"a?b?c?\")",
        "string.match(\"hello\", \"()\", 3)",
        "string.match(\"path/to/file.png\", \"(.+)/\")",
        "string.match(\"path/to/file.png\", \"([^/]+)%.(%w+)$\")",
        "string.match(\"abc\", \".\", -1)",
        "string.gsub(\"hello world\", \"o\", \"0\")",
        "string.gsub(\"hello world\", \"(%w+)\", \"<%1>\")",
        "string.gsub(\"hello\", \"\", \"-\")",
        "string.gsub(\"abc\", \"%w\", \"%0%0\", 2)",
        "string.gsub(\"abc\", \"^a\", \"x\")",
        "string.gsub(\"a b c\", \"%s\", \"%%\")",
        "string.gsub(\"a=1, b=2\", \"(%w+)=(%w+)\", \"%2=%1\")",
        "string.gsub(\"$name is $age\", \"%$(%w+)\", { name = \"x\", age = 3 })",
        "string.gsub(\"$name is $none\", \"%$(%w+)\", { name = false })",
        "string.gsub(\"abc\", \"%w\", function(c) return c:upper() .. \".\" end)",
        "string.gsub(\"abc\", \"(b)\", function() return nil end)",
        "string.gsub(\"abc\", \"()b\", function(p) return p end)",
        "string.gsub(\"x = 1\", \"%s*=%s*\", \"=\")",
        "string.gsub(\"abc\", \".-\", \"-\")",
        "string.gsub(\"abc\", \"x*\", \"-\")",
        // A replacement function that matches too, then returns false or
        // nil: the outer subject is kept.
        "string.gsub(\"ab\", \".\", function() string.match(\"XY\", \"(.)\") return false end)",
        "string.gsub(\"ab\", \".\", function() string.find(\"XY\", \"(.)\") return nil end)",
        "string.gsub(\"ab\", \"(.)\", function(c) return string.gsub(c, \".\", \"%0%0\") end)",
        // 32 captures come back through Lua's own returns.
        "string.find(\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\", \"(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)(a)\")",
    ]);
}

#[test]
fn tonumber_agrees_with_lua_s_own() {
    agrees_with_lua(&[
        "tonumber(\"+7\", 10)",
        "tonumber(\" -12 \", 10)",
        "tonumber(\"\\t+0\\n\", 10)",
        "tonumber(\"99999999999999999999\", 10)",
        "tonumber(\"-9223372036854775808\", 10)",
        "tonumber(\"9223372036854775808\", 10)",
        "tonumber(\"1e2\", 10)",
        "tonumber(\"\", 10)",
        "tonumber(\" \", 10)",
        "tonumber(\"-\", 10)",
        "tonumber(\"+-1\", 10)",
        "tonumber(\"0x10\", 10)",
        "tonumber(\"7a\", 10)",
        "tonumber(\"0x10\")",
        "tonumber(\" 10 \")",
        "tonumber(\"+10\")",
        "tonumber(\"1e2\")",
        "tonumber(\".5\")",
        "tonumber(\"5.\")",
        "tonumber(\"0x1p4\")",
        "tonumber(\"1e\")",
        "tonumber(\"--1\")",
        "tonumber(\"inf\")",
        "tonumber(\"nan\")",
        "tonumber(\"99999999999999999999\")",
        "tonumber(12)",
        "tonumber(1.5)",
        "tonumber({})",
    ]);
}

/// Imports `d` on another thread, failing the test if it takes over a
/// minute (a loop the instruction cap does not see).
#[track_caller]
fn import_within_a_minute(d: TempDir) -> Result<LuaImported, ImportError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(import(&d));
    });
    rx.recv_timeout(std::time::Duration::from_secs(60))
        .expect("the import ran past a minute")
}

#[test]
fn a_candidate_is_refused_once_its_lua_runs_past_its_time() {
    use std::time::{Duration, Instant};

    // How far past the cap the refusal may come: a check's interval, the
    // test's own work and a loaded machine. The passes' carry-over is
    // tested with a set clock (src/lua/tests.rs).
    const SLACK: Duration = Duration::from_secs(10);
    // Each comparison or concatenation is one instruction over 16 MiB:
    // the instruction cap would take hours to stop it, the time cap stops
    // it within a step of its limit.
    for body in [
        "local s, t = rep(\"x\", 1 << 24), rep(\"x\", 1 << 24)\nlocal n = 0\n\
         while true do if s == t then n = n + 1 end end",
        "local s = rep(\"x\", 1 << 24)\nwhile true do local u = s .. \"y\" end",
    ] {
        let start = Instant::now();
        let r = import_within_a_minute(configured(body));
        let took = start.elapsed();
        assert_eq!(why(r), LuaWhy::Limit(LuaLimit::Time), "{body}");
        assert!(
            took >= MAX_LUA_TIME && took < MAX_LUA_TIME + SLACK,
            "{body}: {took:?}"
        );
    }
}

#[test]
fn host_functions_take_a_fixed_number_of_arguments() {
    // Extra arguments are dropped in Lua before a host function is
    // called, so none is converted for it.
    let d = folder(&[
        (
            "s.luaskin",
            &skin(
                "",
                &format!(
                    "local t = {{}}\nfor i = 1, 64 do t[i] = {{ i }} end\n\
                     assert(dofile(\"p.lua\", nil, t, t, t, t, t, t, t) == 7)\n\
                     assert(require(\"p\", t, t, t, t) == 7)\n\
                     assert(skin_config.get_path(\"a.png\", t, t, t) == \"a.png\")\n{DRAW_I}"
                ),
            ),
        ),
        ("p.lua", "return 7"),
    ]);
    import(&d).unwrap();
}

#[test]
fn no_skin_code_runs_once_a_fault_is_raised() {
    // The instruction cap is raised from the hook, where Lua turns hooks
    // off and runs the message handler: the skin's handler is skipped.
    for body in [
        "xpcall(function() while true do end end, function() while true do end end)",
        "xpcall(function() while true do end end, function(e) for i = 1, 1e12 do end return e end)",
        "pcall(xpcall, function() while true do end end, function() while true do end end)",
    ] {
        let r = import_within_a_minute(configured(body));
        assert_eq!(why(r), LuaWhy::Limit(LuaLimit::Instructions), "{body}");
    }
    // An ordinary error still reaches the skin's handler.
    let d = configured(
        "local ok, e = xpcall(error, function(m) return \"handled \" .. m end, \"x\", 0)\n\
         assert(not ok and e == \"handled x\")",
    );
    import(&d).unwrap();
    // Code run while an error unwinds is refused at its metatable.
    let d = configured("local t <close> = setmetatable({}, { __close = function() end })");
    assert_eq!(why(import(&d)), LuaWhy::Finalizer);
}

#[test]
fn natives_never_read_a_skin_s_long_value() {
    // ipairs's step takes an integer, never a numeral string it would
    // parse natively.
    let d = configured(
        "local step, t = ipairs({ 5 })\n\
         local zeros = rep(\"0\", 1 << 23)\n\
         for i = 1, 1000 do assert(not pcall(step, t, zeros)) end\n\
         assert(not pcall(step, t, 0.5))\n\
         local n = 0 for i, v in ipairs({ 1, 2, 3 }) do n = n + i * v end assert(n == 14)\n\
         local p = setmetatable({}, { __index = function(_, i) if i < 4 then return i end end })\n\
         local m = 0 for i in ipairs(p) do m = i end assert(m == 3)",
    );
    let r = import(&d).unwrap();
    assert!(r.usage.native_work < 1 << 20, "{:?}", r.usage);
    // A metatable's `__name` is copied into each tostring and type error:
    // a long one is refused.
    let name = LuaWhy::Library {
        op: "metatable __name over 64 bytes",
    };
    let d = configured("setmetatable({}, { __name = rep(\"n\", 65) })");
    assert_eq!(why(import(&d)), name);
    let d = configured(
        "local t = setmetatable({}, { __name = rep(\"n\", 64) })\n\
         assert(tostring(t):sub(1, 66) == rep(\"n\", 64) .. \": \")\n\
         assert(string.format(\"%.3s\", t) == \"nnn\")",
    );
    import(&d).unwrap();
    // A format's `%s` reads the whole string even when its precision
    // keeps none: the bytes read are charged.
    let d = configured(
        "local s = rep(\"a\", 1 << 23)\nfor i = 1, 20 do string.format(\"%.0s\", s) end",
    );
    assert_eq!(why(import(&d)), LuaWhy::Limit(LuaLimit::NativeWork));
    // An error's message is charged once formatted, before a handler can
    // replace it.
    let d = configured("local s = rep(\"a\", 1 << 23)\nfor i = 1, 20 do pcall(error, s) end");
    assert_eq!(why(import(&d)), LuaWhy::Limit(LuaLimit::NativeWork));
    let d = configured(
        "local s = rep(\"a\", 1 << 23)\n\
         for i = 1, 20 do xpcall(error, function() return false end, s) end",
    );
    assert_eq!(why(import(&d)), LuaWhy::Limit(LuaLimit::NativeWork));
    // A format takes at most 32 conversions.
    let d = configured(
        "local function many(k, ...) if k == 0 then return ... end return many(k - 1, 1, ...) end\n\
         assert(string.format(rep(\"%d\", 32), many(32)) == rep(\"1\", 32))\n\
         pcall(string.format, rep(\"%d\", 33), many(33))",
    );
    assert_eq!(
        why(import(&d)),
        LuaWhy::Library {
            op: "format conversion"
        }
    );
    // Bridge and loader names are measured before they are copied.
    let d = configured("pcall(skin_config.get_path, rep(\"a\", 1025))");
    assert_eq!(
        why(import(&d)),
        LuaWhy::Library {
            op: "bridge argument over the path limit"
        }
    );
    let d = configured("assert(skin_config.get_path(rep(\"a\", 1024)) == rep(\"a\", 1024))");
    import(&d).unwrap();
    for call in ["require", "dofile"] {
        let d = configured(&format!("pcall({call}, rep(\"a\", 1025))"));
        assert_eq!(why(import(&d)), LuaWhy::LoaderName, "{call}");
    }
}

#[test]
fn a_deep_match_is_located_at_the_skin_s_call() {
    // The matcher recurses once a capture: its error is raised many
    // trusted frames below the skin's line.
    let d = configured("\nstring.match(\"\", rep(\"(\", 33))");
    assert_eq!(located(import(&d)), (LuaWhy::Error, "s.luaskin".into(), 6));
}

#[test]
fn a_metatable_s_copy_is_metered() {
    // Each attachment copies the skin's metatable entry by entry as
    // instructions: copying a large one many times reaches the cap.
    let d = configured(
        "local mt = {}\nfor i = 1, 100000 do mt[i] = i end\n\
         for i = 1, 200 do setmetatable({}, mt) end",
    );
    assert_eq!(why(import(&d)), LuaWhy::Limit(LuaLimit::Instructions));
}

#[test]
fn a_skin_s_own_metatable_protection_holds() {
    // A `__metatable` the skin set is its own guard, even one naming the
    // metatable itself: the library never lifts it.
    for mt in [
        "{ __metatable = \"locked\" }",
        "(function() local m = {} m.__metatable = m return m end)()",
    ] {
        let d = configured(&format!(
            "local mt = {mt}\nlocal t = setmetatable({{}}, mt)\n\
             assert(getmetatable(t) == mt.__metatable, \"its guard\")\n\
             assert(not pcall(setmetatable, t, {{}}), \"replaced\")\n\
             assert(not pcall(setmetatable, t, nil), \"removed\")"
        ));
        import(&d).unwrap_or_else(|e| panic!("{mt}: {e:?}"));
    }
}

#[test]
fn protected_calls_and_loads_give_back_at_most_16_results() {
    // The native xpcall pushes its status after the results without
    // making room: results come back packed, and more than fit the free
    // slots of a native call are refused.
    const MANY: &str = "local function many(k, ...) if k == 0 then return ... end return many(k - 1, k, ...) end\n";
    let d = configured(&format!(
        "{MANY}assert(select(\"#\", pcall(many, 16)) == 17 and select(\"#\", xpcall(many, error, 16)) == 17)\n\
         local ok, a, b = pcall(many, 2) assert(ok and a == 1 and b == 2 and select(\"#\", pcall(many, 0)) == 1)\n\
         assert(select(\"#\", string.byte(rep(\"a\", 16), 1, -1)) == 16)"
    ));
    import(&d).unwrap();
    let results = LuaWhy::Library {
        op: "protected call results",
    };
    for body in [
        "pcall(many, 40)",
        "xpcall(many, error, 40)",
        "pcall(many, 17)",
        "xpcall(many, error, 17)",
        "pcall(pcall, many, 16)",
    ] {
        let d = configured(&format!("{MANY}{body}"));
        assert_eq!(why(import(&d)), results, "{body}");
    }
    let values = |n: usize| {
        (1..=n)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    for (n, ok) in [(16, true), (17, false), (40, false)] {
        let d = folder(&[
            (
                "s.luaskin",
                &skin("", &format!("dofile(\"p.lua\")\n{DRAW_I}")),
            ),
            ("p.lua", &format!("return {}", values(n))),
        ]);
        let r = import(&d);
        if ok {
            r.unwrap();
        } else {
            assert_eq!(why(r), results);
        }
    }
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
    let churn = "for i = 1, 400 do local s = rep(\"x\", 1 << 16) .. i end";
    for (attach, change) in [
        (
            "local mt = {}\nlocal t = setmetatable({}, mt)",
            "mt.__mode = \"v\"",
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
        why(common::lua_play(&h, &Config::defaults(h.header()), &cancel)),
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
    fn sync<T: Sync>() {}
    send::<LuaHeader>();
    // Shared by every specialization of a pending skin.
    sync::<LuaHeader>();
    send::<LuaImported>();
    send::<ImportError>();
    let d = folder(&[("s.luaskin", &drawn(""))]);
    let path = d.path().to_owned();
    let h = std::thread::spawn(move || read_lua_header(&path, "s.luaskin", &Cancel::new()))
        .join()
        .unwrap()
        .unwrap();
    let r = std::thread::spawn(move || {
        common::lua_play(&h, &Config::defaults(h.header()), &Cancel::new())
    })
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
    common::lua_play(&h, &Config::defaults(h.header()), &Cancel::new()).unwrap();
    let other = Config {
        options: vec![Some(900)],
        ..Config::default()
    };
    let (w, _, line) = located(common::lua_play(&h, &other, &Cancel::new()));
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
                    "assert(skin_config.file_path.bg == \"b.png\", \"file_path\")\n\
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
                     skin_config.extra = true\n\
                     assert(skin_config.offset.o.x == 123 and skin_config.option.opt == 900, \"written\")\n\
                     assert(skin_config.extra == true, \"extra\")\n{DRAW_I}"
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
    common::lua_play(&h, &config, &Cancel::new()).unwrap();
    common::lua_play(&h, &config, &Cancel::new()).unwrap();
    assert_eq!(config, kept);
}

#[test]
fn a_skin_joins_a_choice_s_folder_and_its_file_path() {
    // A file choice whose sidecar the skin finds by the chosen image's
    // name, and a folder choice whose part it loads from inside.
    let d = folder(&[
        (
            "s.luaskin",
            &skin(
                "filepath = {\n\
                   { name = \"Judge\", path = \"custom/judge/*.png\", def = \"a\" },\n\
                   { name = \"Frame\", path = \"custom/frame/*\" },\n\
                 },",
                &format!(
                    "assert(skin_config.file_path.Judge == \"b.png\", \"judge\")\n\
                     assert(skin_config.file_path.Frame == \"y\", \"frame\")\n\
                     local judge = string.match(skin_config.get_path(\"custom/judge/\" .. skin_config.file_path.Judge), \"(.+)%.png$\") .. \".lua\"\n\
                     local ok, setting = pcall(dofile, judge)\n\
                     assert(ok and setting.offset_x == 7, \"judge sidecar\")\n\
                     local frame = dofile(skin_config.get_path(\"custom/frame/\" .. skin_config.file_path.Frame .. \"/f.lua\"))\n\
                     assert(frame.side == \"y\", \"frame part\")\n{DRAW_I}"
                ),
            ),
        ),
        ("custom/judge/a.png", ""),
        ("custom/judge/a.lua", "return { offset_x = 1 }"),
        ("custom/judge/b.png", ""),
        ("custom/judge/b.lua", "return { offset_x = 7 }"),
        ("custom/frame/x/f.lua", "return { side = \"x\" }"),
        ("custom/frame/y/f.lua", "return { side = \"y\" }"),
    ]);
    let h = header(&d).unwrap();
    let mut config = Config::defaults(h.header());
    config.files.insert("Judge".into(), "b.png".into());
    config.files.insert("Frame".into(), "y".into());
    let r = common::lua_play(&h, &config, &Cancel::new()).unwrap();
    for part in ["custom/judge/b.lua", "custom/frame/y/f.lua"] {
        assert!(r.dependencies.iter().any(|d| d == part), "{part}");
    }
}

#[test]
fn a_path_in_file_path_reaches_only_the_skin_folder() {
    let d = |body: &str| {
        folder(&[
            (
                "s.luaskin",
                &skin(
                    "filepath = { { name = \"Judge\", path = \"custom/judge/*.png\" } },",
                    &format!("{body}\n{DRAW_I}"),
                ),
            ),
            ("custom/judge/a.png", ""),
            ("custom/b.png", ""),
        ])
    };
    // A configured path is no candidate: the choice keeps its first, by
    // name, for both `file_path` and `get_path`.
    let picked = d(
        "assert(skin_config.file_path.Judge == \"a.png\", \"name\")\n\
         assert(skin_config.get_path(\"custom/judge/*.png\") == \"custom/judge/a.png\", \"path\")",
    );
    let h = header(&picked).unwrap();
    for path in ["../b.png", "custom/b.png", "custom/judge/a.png"] {
        let mut config = Config::defaults(h.header());
        config.files.insert("Judge".into(), path.into());
        common::lua_play(&h, &config, &Cancel::new()).unwrap();
    }
    // A path the skin writes in is resolved and loaded in the folder.
    let written = d("skin_config.file_path.Judge = \"../../../x.lua\"\n\
         pcall(dofile, skin_config.get_path(\"custom/judge/\" .. skin_config.file_path.Judge))");
    assert!(matches!(read(import(&written)), RootError::Underflow));
}

#[test]
fn tagged_choices_in_a_lua_skin() {
    // Three choices over one pattern, told apart by their tags: each
    // tagged key takes its own pick, and `file_path` holds the entry's
    // name, never the tag.
    let d = |picks: [&str; 3]| {
        let [cover, lift, hidden] = picks;
        folder(&[
            (
                "s.luaskin",
                &skin(
                    "filepath = {\n\
                       { name = \"Cover\", path = \"parts/*.png|cover|\", def = \"a\" },\n\
                       { name = \"Lift\", path = \"parts/*.png|lift|\", def = \"b\" },\n\
                       { name = \"Hidden\", path = \"parts/*.png|hidden|\", def = \"c\" },\n\
                     },",
                    &format!(
                        "for name, w in pairs({{ Cover = {{ \"cover\", \"{cover}\" }}, \
                             Lift = {{ \"lift\", \"{lift}\" }}, Hidden = {{ \"hidden\", \"{hidden}\" }} }}) do\n\
                           assert(skin_config.file_path[name] == w[2], name .. \" file_path\")\n\
                           assert(skin_config.get_path(\"parts/*.png|\" .. w[1] .. \"|\") == \"parts/\" .. w[2], name .. \" get_path\")\n\
                         end\n{DRAW_I}"
                    ),
                ),
            ),
            ("parts/a.png", ""),
            ("parts/b.png", ""),
            ("parts/c.png", ""),
        ])
    };
    let defaults = d(["a.png", "b.png", "c.png"]);
    let h = header(&defaults).unwrap();
    assert!(
        h.header()
            .files
            .iter()
            .all(|f| f.candidates == ["a.png", "b.png", "c.png"])
    );
    common::lua_play(&h, &Config::defaults(h.header()), &Cancel::new()).unwrap();
    // Configuring one tagged choice changes only it.
    let lifted = d(["a.png", "c.png", "c.png"]);
    let h = header(&lifted).unwrap();
    let mut config = Config::defaults(h.header());
    config.files.insert("Lift".into(), "c.png".into());
    common::lua_play(&h, &config, &Cancel::new()).unwrap();
}

#[test]
fn get_path_substitutes_by_the_source_formula() {
    // The first declared key that prefixes the path substitutes; the
    // result is the path up to its last `*`, the pick, and the path after
    // the key, returned as it is.
    let d = |choices: &str, body: &str| {
        folder(&[
            (
                "s.luaskin",
                &skin(
                    &format!("filepath = {{ {choices} }},"),
                    &format!("{body}\n{DRAW_I}"),
                ),
            ),
            ("parts/a.png", ""),
            ("parts/b.png", ""),
            ("pre/preone.png", ""),
        ])
    };
    let untagged_first = d(
        "{ name = \"All\", path = \"parts/*.png\", def = \"a\" },\n\
         { name = \"Lift\", path = \"parts/*.png|lift|\", def = \"b\" },",
        "assert(skin_config.get_path(\"parts/*.png\") == \"parts/a.png\", \"untagged\")\n\
         assert(skin_config.get_path(\"parts/*.png|lift|\") == \"parts/a.png|lift|\", \"tag kept\")\n\
         assert(skin_config.file_path.Lift == \"b.png\", \"own pick\")",
    );
    let tagged_first = d(
        "{ name = \"Lift\", path = \"parts/*.png|lift|\", def = \"b\" },\n\
         { name = \"All\", path = \"parts/*.png\", def = \"a\" },\n\
         { name = \"Pre\", path = \"pre/pre*.png|a|\" },",
        "assert(skin_config.get_path(\"parts/*.png|lift|\") == \"parts/b.png\", \"tagged\")\n\
         assert(skin_config.get_path(\"parts/*.png\") == \"parts/a.png\", \"untagged\")\n\
         assert(skin_config.get_path(\"parts/*.png|lift|/*/x\") == \"parts/*.png|lift|/b.png/*/x\", \"last star\")\n\
         assert(skin_config.file_path.Pre == \"preone.png\", \"prefix pick\")\n\
         assert(skin_config.get_path(\"pre/pre*.png|a|\") == \"pre/prepreone.png\", \"literal prefix\")",
    );
    for d in [untagged_first, tagged_first] {
        let h = header(&d).unwrap();
        common::lua_play(&h, &Config::defaults(h.header()), &Cancel::new()).unwrap();
    }
}

#[test]
fn a_directory_choice_loads_its_selected_part() {
    // Folders are candidates beside files; the default's first match by
    // name or stem is the folder `y`, sorted before the file `y.lua`.
    let d = folder(&[
        (
            "s.luaskin",
            &skin(
                "filepath = { { name = \"Frame\", path = \"custom/frame/*\", def = \"y\" } },",
                &format!(
                    "assert(skin_config.file_path.Frame == \"y\", \"frame\")\n\
                     local frame = dofile(skin_config.get_path(\"custom/frame/*\") .. \"/f.lua\")\n\
                     assert(frame.side == \"y\", \"frame part\")\n{DRAW_I}"
                ),
            ),
        ),
        ("custom/frame/x/f.lua", "return { side = \"x\" }"),
        ("custom/frame/y/f.lua", "return { side = \"y\" }"),
        ("custom/frame/y.lua", "return { side = \"file\" }"),
    ]);
    let h = header(&d).unwrap();
    assert_eq!(h.header().files[0].candidates, ["x", "y", "y.lua"]);
    let r = common::lua_play(&h, &Config::defaults(h.header()), &Cancel::new()).unwrap();
    assert!(
        r.dependencies.iter().any(|d| d == "custom/frame/y/f.lua"),
        "{:?}",
        r.dependencies
    );
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
           assert(require(\"main_state\").offset == nil, \"offset\")\n\
           return { type = 0 }\n\
         end\n\
         local ms = require(\"main_state\")\n\
         assert(ms.timer_off_value == -9223372036854775807 - 1, \"off value\")\n\
         assert(ms.event_observe_boolean == nil and ms.lua_version == nil, \"absent\")\n\
         assert(require(\"event_util\").event_observe_boolean == nil, \"absent event\")\n\
         return { type = 0 }",
    )]);
    let h = header(&d).unwrap();
    // Each pinned name is a function in its module, and calling it is
    // refused as reading play state; `offset` answers from the
    // configuration (`offset_answers_the_configuration`).
    for (index, &(module, api)) in HOST_APIS.iter().enumerate() {
        if api == "offset" {
            continue;
        }
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
        let w = why(common::lua_play(
            &h,
            &Config::defaults(h.header()),
            &Cancel::new(),
        ));
        assert_eq!(
            w,
            LuaWhy::LiveState {
                api: u16::try_from(index).unwrap()
            },
            "{module}.{api}"
        );
    }
    common::lua_play(&h, &Config::defaults(h.header()), &Cancel::new()).unwrap();
}

#[test]
fn every_function_in_the_definition_is_classified_and_consumed_where_read() {
    // A write-free condition converts, one with no result too, as the
    // false its missing result is, wherever it is.
    let d = folder(&[("s.luaskin", &drawn("draw = function() return true end,"))]);
    import(&d).unwrap();
    for fields in ["draw = function() end,", "op = { function() end },"] {
        let d = folder(&[("s.luaskin", &drawn(fields))]);
        let i = import(&d).unwrap();
        assert_eq!(i.callbacks, [Disposition::Lowered], "{fields}");
        assert!(
            common::play::Drawn::new(i.imported.skin, 0)
                .frame(1_000, &[])
                .is_empty(),
            "{fields}"
        );
    }
    // A function in a field that takes none, which the source never
    // reads, is inert, and refused where the import reads it, at its
    // definition.
    let d = folder(&[("s.luaskin", &drawn("blend = function() return 1 end,"))]);
    assert_eq!(
        located(import(&d)),
        (LuaWhy::Callable { field: "blend" }, "s.luaskin".into(), 5)
    );
    // A gauge's condition and a number's value convert.
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
    import(&d).unwrap();
    let d = folder(&[(
        "s.luaskin",
        &skin(
            "",
            "skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 3,\n\
               value = function() return 1 end } }\n\
             skin.destination = { { id = \"v\", dst = { { w = 6, h = 6 } } } }",
        ),
    )]);
    import(&d).unwrap();
    // A root nothing consumes is classified too; a function the source
    // never reads is inert.
    let d = configured("skin.customEvents = { { id = 1000, action = function() end } }");
    assert_eq!(import(&d).unwrap().callbacks, [Disposition::ClassifiedOnly]);
    let d = configured("skin.unread = { function() end }");
    assert_eq!(import(&d).unwrap().callbacks, [Disposition::Inert]);
    // A module's function is consumed like the entry's.
    let d = folder(&[
        ("s.luaskin", &drawn("draw = require(\"f\"),")),
        ("f.lua", "local x = 1\nreturn function() end"),
    ]);
    assert_eq!(import(&d).unwrap().callbacks, [Disposition::Lowered]);
    // A host function has no tree: it opens the analysis, and nothing
    // consumes it.
    let d = configured(
        "skin.customEvents = { { id = 1000, action = require(\"timer_util\").now_timer } }",
    );
    assert_eq!(import(&d).unwrap().callbacks, [Disposition::ClassifiedOnly]);
}

/// A function as an item of `bpmgraph`, `hiddenCover`, `liftCover` or
/// `pmchara`, or as a branch of a record choice there, is refused where
/// the import reads it, as in every other object array.
#[test]
fn a_function_in_an_object_array_is_refused() {
    for name in ["bpmgraph", "hiddenCover", "liftCover", "pmchara"] {
        for items in ["{ function() end }", "{ { function() end } }"] {
            let d = configured(&format!("skin.{name} = {items}"));
            assert_eq!(
                why(import(&d)),
                LuaWhy::Callable { field: "other" },
                "{name} {items}"
            );
        }
    }
}

#[test]
fn the_header_pass_refuses_a_function_only_where_it_reads() {
    // Where it does not: a field outside the header. The header pass
    // reads past it; the configured pass classifies a root there, which
    // nothing consumes.
    for fields in [
        "customEvents = { { id = 1000,\n action = function() end } }",
        "w = 1920, h = 1080, destination = { { id = \"i\",\n draw = function() end } }",
    ] {
        let d = folder(&[("s.luaskin", &format!("return {{ type = 0, {fields} }}"))]);
        let h = header(&d).unwrap_or_else(|e| panic!("{fields}: {e:?}"));
        let i = common::lua_play(&h, &Config::defaults(h.header()), &Cancel::new())
            .unwrap_or_else(|e| panic!("{fields}: {e:?}"));
        assert_eq!(i.callbacks, [Disposition::ClassifiedOnly], "{fields}");
    }
    // A key of an option item that neither the header nor the source
    // reads, or a field outside the source's skin: the header pass reads
    // past it, and the configured pass leaves it inert.
    for fields in [
        "property = { { name = \"o\", item = { { name = \"a\", op = 900,\n is_selected = function() end } } } }",
        "w = 1920, h = 1080,\n helper = function() end",
    ] {
        let d = folder(&[("s.luaskin", &format!("return {{ type = 0, {fields} }}"))]);
        let h = header(&d).unwrap_or_else(|e| panic!("{fields}: {e:?}"));
        let i = common::lua_play(&h, &Config::defaults(h.header()), &Cancel::new())
            .unwrap_or_else(|e| panic!("{fields}: {e:?}"));
        assert_eq!(i.callbacks, [Disposition::Inert], "{fields}");
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
        input(common::lua_play(
            &h,
            &Config::defaults(h.header()),
            &Cancel::new()
        )),
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
        input(common::lua_play(
            &h,
            &Config::defaults(h.header()),
            &Cancel::new()
        )),
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
    std::fs::write(d.path().join("p.json"), "[{").unwrap();
    common::lua_play(&h, &Config::defaults(h.header()), &Cancel::new()).unwrap();
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
    for value in [
        "4294967297",
        "-2147483649",
        "0/0",
        "1/0",
        "-9223372036854775807 - 1",
    ] {
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
    // Without a string key, only `1..=n` is a shape: sparse integers, and
    // keys that are neither strings nor integers, are refused.
    for (body, want) in [
        ("skin.extra = { 1, nil, 3 }", LuaWhy::Sparse),
        ("skin.extra = { [1] = 1, [3] = 2 }", LuaWhy::Sparse),
        ("skin.extra = { [0] = 1 }", LuaWhy::Sparse),
        ("skin.extra = { [1.5] = 1 }", LuaWhy::Key),
        ("skin.extra = { 1, [1.5] = 2 }", LuaWhy::Key),
        ("skin.extra = { [true] = 1 }", LuaWhy::Key),
        ("skin.extra = skin", LuaWhy::Cycle),
        ("skin.extra = \"\\xff\"", LuaWhy::NotUtf8),
    ] {
        assert_eq!(why(import(&configured(body))), want, "{body}");
    }
    // A string key beside them makes a mixed table, which an unread
    // field holds with one note.
    let r = import(&configured("skin.extra = { 1, 2, x = 3 }")).unwrap();
    assert_eq!(r.imported.report.usage.elements, 1);
    assert_eq!(kinds(&r), [NON_STRING_KEYS]);
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
    let long = configured("skin.extra = rep(\"x\", (64 << 10) + 1)");
    assert_eq!(input(import(&long)), InputLimit::StringBytes);
    let many = configured(
        "local s = rep(\"x\", 64 << 10)\nlocal t = {}\n\
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

#[test]
fn tagging_keeps_statement_boundaries() {
    // A constructor that ends a statement still ends it once tagged: the
    // `(` that starts the next statement calls nothing of it.
    for body in [
        "local function identity(f) changed = true return f end\n(function() return true end)()",
        "local g = function() changed = true end\n(function() return true end)()",
        "h = function() changed = true end\n(function() return true end)()",
        "local t = {}\nfunction t.m() changed = true end\n(function() return true end)()",
        "local u = 1, function() changed = true end\n(function() return true end)()",
        "repeat local r = 1 until function() changed = true end\n(function() return true end)()",
    ] {
        let d = configured(&format!(
            "local changed = false\n{body}\nassert(not changed, \"changed\")"
        ));
        import(&d).unwrap_or_else(|e| panic!("{body}: {e:?}"));
    }
}

#[test]
fn the_header_pass_gives_the_kind_the_type_names() {
    for (ty, kind) in [
        (0, ScreenKind::Play),
        (5, ScreenKind::Selection),
        (6, ScreenKind::Decision),
        (7, ScreenKind::Result),
    ] {
        let d = folder(&[("s.luaskin", &format!("return {{ type = {ty} }}"))]);
        assert_eq!(header(&d).unwrap().header().kind, kind, "{ty}");
    }
    let d = folder(&[("s.luaskin", "return { type = 1 }")]);
    assert!(matches!(
        header(&d),
        Err(ImportError::UnsupportedKind { ty: Some(1), .. })
    ));
}

/// A play skin whose configured pass sets `skin.type` to `ty`, then runs
/// `body`.
fn retyped(ty: i32, body: &str) -> TempDir {
    configured(&format!("skin.type = {ty}\n{body}"))
}

/// A function the callback admission refuses: its walk passes the depth
/// cap.
fn unsupported() -> String {
    let chain = vec!["1"; 300].join("+");
    format!("skin.customEvents = {{ {{ action = function() return {chain} end }} }}")
}

/// Checks `run`, an import into the play slot, on a play skin whose
/// configured pass returns type 7, also with a callback the admission
/// refuses: a refusal located at the configured table's `type`, where an
/// unsupported type 3 is refused too.
fn refuses_a_retype(run: impl Fn(&TempDir) -> Result<(), ImportError>) {
    // The callback alone refuses the skin.
    assert!(matches!(
        run(&retyped(0, &unsupported())),
        Err(ImportError::Lua { .. })
    ));
    for body in [String::new(), unsupported()] {
        let body = body.as_str();
        let at = match run(&retyped(3, body)) {
            Err(ImportError::UnsupportedKind {
                at: Some(at),
                ty: Some(3),
            }) => at,
            other => panic!("{body}: {other:?}"),
        };
        match run(&retyped(7, body)) {
            Err(ImportError::Slot {
                at: Some(a),
                slot: ScreenKind::Play,
                found: ScreenKind::Result,
            }) => assert_eq!(a, at, "{body}"),
            other => panic!("{body}: {other:?}"),
        }
    }
}

#[test]
fn a_configured_pass_of_another_type_refuses_at_its_type() {
    refuses_a_retype(|d| import(d).map(|_| ()));
}

#[cfg(feature = "dev-tools")]
#[test]
fn a_timed_configured_pass_of_another_type_refuses_at_its_type() {
    refuses_a_retype(|d| {
        let h = header(d).unwrap();
        let facts = skin_beatoraja::ImportFacts::unavailable(ScreenKind::Play);
        h.timed(&Config::defaults(h.header()), &facts, &Cancel::new())
            .map(|_| ())
    });
}

/// The note a table with string keys and others gets.
const NON_STRING_KEYS: NoteKind = NoteKind::Ignored {
    field: "non-string keys",
};

/// The kinds of `r`'s notes.
fn kinds(r: &LuaImported) -> Vec<NoteKind> {
    common::kinds(&r.imported.report)
}

/// A destination's `dst` drawing image `i` once.
const DST: &str = "dst = { { x = 0, y = 0, w = 64, h = 64 } }";

#[test]
fn a_mixed_table_is_a_record_of_its_string_keys() {
    // Its other entries are never read, whatever their key: the image
    // draws as without them, with one ignored note.
    let image = |extra: &str| {
        configured(&format!(
            "skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64{extra} }} }}"
        ))
    };
    // Each folder is kept until its atlas is built.
    let d = image("");
    let base = import(&d).unwrap();
    let usage = base.imported.report.usage;
    let want = common::play::Drawn::new(base.imported.skin, 100).rects(1_000);
    assert_eq!(want.len(), 1);
    for extra in [", 41", ", [1.5] = 2", ", [true] = 1"] {
        let d = image(extra);
        let r = import(&d).unwrap();
        let report = &r.imported.report;
        assert_eq!(kinds(&r), [NON_STRING_KEYS], "{extra}");
        assert_eq!(report.notes[0].severity, Severity::Ignored);
        assert!(report.notes[0].at.is_some());
        assert_eq!(
            (report.usage.images, report.usage.frames),
            (usage.images, usage.frames)
        );
        assert_eq!(report.usage.file_attempts, usage.file_attempts);
        let rects = common::play::Drawn::new(r.imported.skin, 100).rects(1_000);
        assert_eq!(rects, want, "{extra}");
    }
}

#[test]
fn a_mixed_table_has_no_wrapper_meaning() {
    // An `include` beside a positional entry is a field like any other:
    // the record is drawn as a failure-timed destination, and the absent
    // `empty.json` is never read.
    let failure = |extra: &str| folder(&[("s.luaskin", &drawn(&format!("timer = 3{extra}, ")))]);
    let plain = import(&failure("")).unwrap();
    let r = import(&failure(", include = \"empty.json\", 41")).unwrap();
    assert_eq!(kinds(&r), [NON_STRING_KEYS]);
    assert_eq!(r.imported.report.usage.elements, 1);
    assert_eq!(
        r.imported.report.usage.elements,
        plain.imported.report.usage.elements
    );
    assert_eq!(
        r.imported.report.usage.file_attempts,
        plain.imported.report.usage.file_attempts
    );
    assert_eq!(r.dependencies, plain.dependencies);
    assert_eq!(r.dependencies, ["s.luaskin"]);
    // Nor are `if`, `value` and `values`: read as a choice, the condition
    // on option 900 (not enabled) would drop the record; as fields, the
    // record itself is drawn.
    let other = "{ id = \"i\", dst = { { x = 100, y = 0, w = 8, h = 8 } } }";
    for wrapper in [
        format!("[\"if\"] = {{ 900 }}, value = {other}"),
        format!("[\"if\"] = {{ 900 }}, values = {{ {other} }}"),
        format!("IF = {{ 900 }}, Value = {other}"),
    ] {
        let r = import(&failure(&format!(", {wrapper}, 41"))).unwrap();
        assert_eq!(kinds(&r), [NON_STRING_KEYS], "{wrapper}");
        assert_eq!(r.imported.report.usage.elements, 1, "{wrapper}");
        assert_eq!(r.imported.report.usage.keys, 1, "{wrapper}");
    }
}

#[test]
fn a_mixed_keyframe_is_a_record_with_defaults() {
    // The first keyframe keeps its string field and defaults the rest
    // (time 0, zero geometry): at 500 ms the image is halfway to its
    // second keyframe, with and without the failure timer.
    for timer in ["", "timer = 3, "] {
        let d = folder(&[(
            "s.luaskin",
            &skin(
                "",
                &format!(
                    "skin.destination = {{ {{ id = \"i\", loop = -1, {timer}dst = {{ {{ x = 1, 41 }},\n\
                       {{ time = 1000, x = 0, y = 0, w = 64, h = 64 }} }} }} }}"
                ),
            ),
        )]);
        let r = import(&d).unwrap();
        assert_eq!(kinds(&r), [NON_STRING_KEYS], "{timer}");
        assert_eq!(r.imported.report.usage.keys, 2, "{timer}");
        let want: &[[i32; 4]] = if timer.is_empty() {
            &[[1, 1048, 32, 32]]
        } else {
            // The failure timer never starts in a plain play.
            &[]
        };
        let mut drawn = common::play::Drawn::new(r.imported.skin, 100);
        assert_eq!(drawn.rects(500), want, "{timer}");
    }
}

#[test]
fn a_mixed_table_s_other_values_are_never_converted() {
    // A closure the admission refuses where it is read.
    let closure = "function() return ms.number(110) ^ 2 > 0 end";
    let body = |record: &str| {
        folder(&[(
            "s.luaskin",
            &skin(
                "",
                &format!(
                    "local ms = require(\"main_state\")\n\
                     skin.destination = {{ {record} }}"
                ),
            ),
        )])
    };
    let read = import(&body(&format!(
        "{{ id = \"i\", timer = 3, draw = {closure}, {DST} }}"
    )));
    assert!(read.is_err(), "{read:?}");
    // Under a positional key it is never read: no callable is recorded.
    let r = import(&body(&format!(
        "{{ id = \"i\", timer = 3, {DST}, {closure} }}"
    )))
    .unwrap();
    assert!(r.callbacks.is_empty(), "{:?}", r.callbacks);
    assert_eq!(kinds(&r), [NON_STRING_KEYS]);
    assert_eq!(r.imported.report.usage.elements, 1);
}

#[test]
fn mixed_table_notes_are_capped() {
    let d = configured("skin.extra = {}\nfor k = 1, 4097 do skin.extra[k] = { 1, x = 1 } end");
    let r = import(&d).unwrap();
    let report = &r.imported.report;
    assert_eq!(report.notes.len(), 4_096);
    assert!(report.notes.iter().all(|n| n.kind == NON_STRING_KEYS));
    assert_eq!(report.suppressed, 1);
    assert_eq!(report.usage.elements, 1);
}

#[test]
fn a_mixed_table_where_an_array_or_condition_is_read_is_refused() {
    let entry = |body: &str| skin("", body);
    let drawn_after = |body: &str| skin("", &format!("{body}\n{DRAW_I}"));
    for body in [
        // An array field, and one with an `include` key.
        entry(&format!(
            "skin.destination = {{ {{ id = \"i\", {DST} }}, n = 1 }}"
        )),
        entry(&format!(
            "skin.destination = {{ {{ id = \"i\", {DST} }}, include = \"x.json\" }}"
        )),
        // A destination's keys.
        entry(
            "skin.destination = { { id = \"i\", dst = { { x = 0, y = 0, w = 64, h = 64 }, foo = 1 } } }",
        ),
        // A literal array.
        drawn_after(
            "skin.liftCover = { { id = \"l\", src = \"a\", x = 0, y = 0, w = 8, h = 8 }, n = 1 }",
        ),
        // A value's offset rows.
        drawn_after(
            "skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10,\n\
               digit = 1, ref = 71, offset = { { x = 1 }, n = 1 } } }",
        ),
        // A condition, a condition's item, and an item's alternative.
        entry(&format!(
            "skin.destination = {{ {{ [\"if\"] = {{ 900, x = 1 }}, value = {{ id = \"i\", {DST} }} }} }}"
        )),
        entry(&format!(
            "skin.destination = {{ {{ [\"if\"] = {{ {{ 900, x = 1 }} }}, value = {{ id = \"i\", {DST} }} }} }}"
        )),
        entry(&format!(
            "skin.destination = {{ {{ [\"if\"] = {{ {{ {{ 900, x = 1 }} }} }}, value = {{ id = \"i\", {DST} }} }} }}"
        )),
        // A branch of a choice list, which would otherwise select nothing.
        entry(&format!(
            "skin.destination = {{ {{ {{ value = {{ id = \"i\", {DST} }}, 41 }} }} }}"
        )),
    ] {
        // `x.json`, were it read, is no JSON.
        let d = folder(&[("s.luaskin", &body), ("x.json", "not json")]);
        assert_eq!(why(import(&d)), LuaWhy::Mixed, "{body}");
    }
    // The header's options.
    let d = folder(&[(
        "s.luaskin",
        &skin(
            "property = { { name = \"o\", item = { { name = \"a\", op = 900 } } }, n = 1 },",
            DRAW_I,
        ),
    )]);
    assert_eq!(why(header(&d)), LuaWhy::Mixed);
    // A reader of its own refuses with its type error.
    let d = folder(&[(
        "s.luaskin",
        &entry(&format!(
            "skin.destination = {{ {{ id = \"i\", op = {{ 900, x = 1 }}, {DST} }} }}"
        )),
    )]);
    let r = import(&d);
    assert!(
        matches!(r, Err(ImportError::Type { field: "op", .. })),
        "{r:?}"
    );
}

#[test]
fn a_lua_cell_no_atlas_page_holds_drops_its_element_alone() {
    // A whole-file frame drawn 1×1 beside its sheet's fitting frames, as a
    // JSON skin's is.
    let d = common::skin(&[(
        "s.luaskin",
        &skin(
            "",
            "skin.source = { { id = \"w\", path = \"w.png\" } }\n\
             skin.image = {\n\
               { id = \"whole\", src = \"w\", x = 0, y = 0, w = -1, h = -1 },\n\
               { id = \"frames\", src = \"w\", x = 0, y = 0, w = 8177, h = 2, divx = 4 },\n\
             }\n\
             skin.destination = {\n\
               { id = \"whole\", dst = { { x = 0, y = 0, w = 1, h = 1 } } },\n\
               { id = \"frames\", dst = { { x = 0, y = 0, w = 64, h = 64 } } },\n\
             }",
        ),
    )]);
    common::png(d.path(), "w.png", 8_177, 2);
    let r = import(&d).unwrap();
    assert_eq!(kinds(&r), [NoteKind::CellTooLarge { w: 8_177, h: 2 }]);
    assert_eq!(r.imported.report.notes[0].severity, Severity::Dropped);
    assert_eq!(r.imported.report.usage.files, 1);
    // The fitting frames are what remains, drawn from a built atlas.
    let skin = r.imported.skin;
    let limits = common::play::LIMITS;
    assert!(
        skin.atlas(skin_model::Variant::Lo, limits, &|| false)
            .is_ok()
    );
    assert_eq!(common::play::Drawn::new(skin, 10).frame(0, &[]).len(), 1);
}

/// `json`, a JSON value of objects, arrays, strings and numbers, as a
/// Lua table constructor.
fn lua_table(json: &str) -> String {
    let json = json.replace('[', "{").replace(']', "}");
    let mut out = String::new();
    let mut rest = json.as_str();
    while let Some(open) = rest.find('"') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after.find('"').unwrap();
        let s = &after[..close];
        rest = &after[close + 1..];
        match rest.trim_start().strip_prefix(':') {
            Some(r) => {
                out.push_str(&format!("[\"{s}\"] ="));
                rest = r;
            }
            None => out.push_str(&format!("\"{s}\"")),
        }
    }
    out.push_str(rest);
    out
}

#[test]
fn a_mixed_note_part_array_is_refused() {
    // Lanes with distinct active (`deco`) and inactive bodies; `LNBA`
    // and `HID` stand for `lnbodyActive` and `hidden`.
    let json = format!(
        r#"{{"type": 0, "w": 1920, "h": 1080, "source": [{{"id": "a", "path": "a.png"}}], {}}}"#,
        common::lanes::body(
            "",
            &common::lanes::note(r#""lnbodyActive": LNBA, "hidden": HID"#),
            &[common::lanes::NOTES]
        )
    );
    let lua = format!("return {}", lua_table(&json));
    let deco = "\"deco\", ".repeat(8);
    let lanes = |lnba: &str, hid: &str| {
        folder(&[("s.luaskin", &lua.replace("LNBA", lnba).replace("HID", hid))])
    };
    // As arrays, they import.
    let r = import(&lanes(&format!("{{ {deco} }}"), "{}")).unwrap();
    assert!(!kinds(&r).contains(&NON_STRING_KEYS), "{:?}", kinds(&r));
    // The body probe never reads it as empty, which would select the
    // inactive bodies.
    let mixed = lanes(&format!("{{ {deco} n = 8 }}"), "{}");
    assert_eq!(why(import(&mixed)), LuaWhy::Mixed);
    let mixed = lanes(&format!("{{ {deco} }}"), "{ \"x\", n = 1 }");
    assert_eq!(why(import(&mixed)), LuaWhy::Mixed);
}

/// A header declaring offsets 71 (`G`), 72 (`H`) and 73 (no name).
const OFFSETS: &str = "offset = { { name = \"G\", id = 71, x = true, y = true, w = true, h = true, r = true, a = true },\n\
    { name = \"H\", id = 72, w = true }, { id = 73, x = true } },";

/// The configured pass's `main_state.offset(id)`, its six fields written
/// out as `tostring` writes them (`3`, an integer; `3.0`, a float).
const FIELDS: &str = "local ms = require(\"main_state\") \
    local function f(id) local t = ms.offset(id) \
    return tostring(t.x) .. \",\" .. tostring(t.y) .. \",\" .. tostring(t.w) .. \",\" \
    .. tostring(t.h) .. \",\" .. tostring(t.r) .. \",\" .. tostring(t.a) end";

/// [`Config::defaults`] of `h` with offsets 71 = (1..6), 73 = (7..12) and
/// 74 = 9s; 74 is not declared.
fn offsets_configured(h: &LuaHeader) -> Config {
    let mut c = Config::defaults(h.header());
    let v = |a: [i32; 6]| OffsetValue {
        x: a[0],
        y: a[1],
        w: a[2],
        h: a[3],
        r: a[4],
        a: a[5],
    };
    c.offsets.insert(71, v([1, 2, 3, 4, 5, 6]));
    c.offsets.insert(73, v([7, 8, 9, 10, 11, 12]));
    c.offsets.insert(74, v([9; 6]));
    c
}

/// s7-06f-design.md F1, F2: the configured pass's `main_state.offset(n)`
/// is the configured offset of a declared id, named or not, as integers;
/// zeros for an unconfigured declared id and for any undeclared id in 0 to
/// 199, a configured one included.
#[test]
fn offset_answers_the_configuration() {
    let d = folder(&[(
        "s.luaskin",
        &skin(
            OFFSETS,
            &format!(
                "{FIELDS}\n\
                 assert(f(71) == \"1,2,3,4,5,6\", \"71 \" .. f(71))\n\
                 assert(f(73) == \"7,8,9,10,11,12\", \"73 \" .. f(73))\n\
                 for _, id in ipairs({{ 72, 0, 199, 74 }}) do\n\
                   assert(f(id) == \"0,0,0,0,0,0\", id .. \" \" .. f(id))\n\
                 end\n{DRAW_I}"
            ),
        ),
    )]);
    let h = header(&d).unwrap();
    common::lua_play(&h, &offsets_configured(&h), &Cancel::new()).unwrap();
}

/// s7-06f-design.md F3: an id past the source's array, or an argument that
/// is not a Lua integer, refuses the skin at its call, also inside
/// `pcall`: the refusal is a recorded fault, not an error Lua recovers.
#[test]
fn offset_outside_its_range_or_not_an_integer_refuses_the_skin() {
    for arg in ["-1", "200", "71.5", "71.0", "\"71\"", "nil", "true"] {
        for call in [
            format!("ms.offset({arg})"),
            format!("local ok = pcall(ms.offset, {arg})"),
        ] {
            let d = folder(&[(
                "s.luaskin",
                &skin(
                    OFFSETS,
                    &format!("local ms = require(\"main_state\") {call}\n{DRAW_I}"),
                ),
            )]);
            let h = header(&d).unwrap();
            let r = common::lua_play(&h, &offsets_configured(&h), &Cancel::new());
            assert_eq!(
                located(r),
                (LuaWhy::FactArgument, "s.luaskin".into(), 6),
                "{call}"
            );
        }
    }
}

/// s7-06f-design.md F4: each call builds a new table; writing to one
/// changes neither the next answer nor `skin_config.offset`.
#[test]
fn offset_tables_are_fresh() {
    let d = folder(&[(
        "s.luaskin",
        &skin(
            OFFSETS,
            &format!(
                "local ms = require(\"main_state\")\n\
                 local t = ms.offset(71)\n\
                 t.w = 99\n\
                 assert(ms.offset(71).w == 3, \"fresh\")\n\
                 assert(skin_config.offset[\"G\"].w == 3, \"configuration\")\n{DRAW_I}"
            ),
        ),
    )]);
    let h = header(&d).unwrap();
    common::lua_play(&h, &offsets_configured(&h), &Cancel::new()).unwrap();
}
