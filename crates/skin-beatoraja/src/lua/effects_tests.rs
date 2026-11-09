//! The effect walk and what it does to lowering (design.md "beatoraja Lua
//! Skins", effects by role): a candidate is closed exactly when every
//! root stays inside the modeled subset, a construct outside it opens the
//! candidate at its place, and a root that reaches a write never lowers.
//! Each test imports through the candidate's two passes
//! ([`super::callback_tests::outcome`]).

use super::callback_tests::{gauge, image, outcome, reader_entry, skin, writer_chunk};
use super::{Disposition, LuaWhy, Unsupported};
use crate::error::ImportError;
use crate::import::callback::LOWERED;

/// A critical gauge `draw` reading option `x`, which [`beside`] sets.
const READER: &str = "function() return skin_config.option[\"x\"] == 1 end";

/// A skin whose configured pass sets option `x` to 1, runs `pre`, then
/// binds `d` to `draw` on its own line ([`D_LINE`] after `pre`'s lines),
/// with a decorative image drawn under `d` before the critical reader.
fn beside(pre: &str, draw: &str) -> String {
    skin(
        &format!("skin_config.option.x = 1\n{pre}\nlocal d = {draw}"),
        &format!("{}{}", image("d"), gauge(READER)),
    )
}

/// The line `d` is bound on: the template's three lines, the option's,
/// then `pre`'s.
fn d_line(pre: &str) -> u32 {
    4 + u32::try_from(pre.lines().count().max(1)).unwrap() + 1
}

/// Checks that `d` opens the candidate at its own line: the critical
/// reader refuses there for [`Unsupported::Interference`], and `d`, when
/// `writes`, is replaced for [`Unsupported::Writes`].
#[track_caller]
fn opens(pre: &str, draw: &str, writes: bool) {
    let (r, callbacks) = outcome(&beside(pre, draw), &[]);
    match r {
        Err(ImportError::Lua {
            why:
                LuaWhy::Critical {
                    field: "draw",
                    why: Unsupported::Interference,
                },
            at: Some(at),
        }) => assert_eq!(
            (at.file.as_str(), at.line),
            ("s.luaskin", d_line(pre)),
            "{pre} / {draw}"
        ),
        other => panic!("{pre} / {draw}: {other:?}"),
    }
    let first = callbacks.first().copied();
    if writes {
        assert_eq!(
            first,
            Some(Disposition::Replaced(Unsupported::Writes)),
            "{pre} / {draw}"
        );
    } else {
        // Replaced for its own construct, or lowered when it needs no
        // captured state.
        assert!(
            matches!(first, Some(Disposition::Replaced(w)) if w != Unsupported::Writes)
                || first == Some(Disposition::Lowered),
            "{pre} / {draw}: {first:?}"
        );
    }
}

/// Checks that the candidate stays closed beside `d`: the critical reader
/// lowers, the option folded.
#[track_caller]
fn closed(pre: &str, draw: &str) {
    let (r, callbacks) = outcome(&beside(pre, draw), &[]);
    r.unwrap_or_else(|e| panic!("{pre} / {draw}: {e:?}"));
    assert_eq!(
        callbacks.get(1),
        Some(&Disposition::Lowered),
        "{pre} / {draw}: {callbacks:?}"
    );
}

#[test]
fn a_frame_time_writer_of_the_option_path_voids_every_fold() {
    // Fixture 1: `w` replaces `skin_config.option` through an alias. The
    // decorative caller is a writer, replaced; the critical reader's fold
    // is void, refused at the assignment; nothing lowers.
    LOWERED.with(|l| l.borrow_mut().clear());
    let a = writer_chunk("cfg.option = { x = 2 }");
    let (r, callbacks) = outcome(&reader_entry(), &[("a.lua", &a)]);
    match r {
        Err(ImportError::Lua {
            why:
                LuaWhy::Critical {
                    field: "draw",
                    why: Unsupported::Interference,
                },
            at: Some(at),
        }) => assert_eq!((at.file.as_str(), at.line), ("a.lua", 3)),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        callbacks,
        [
            Disposition::Replaced(Unsupported::Writes),
            Disposition::Refused
        ]
    );
    assert!(LOWERED.with(|l| l.borrow().is_empty()));
    // The same chain without the write: both lower, the option folded.
    LOWERED.with(|l| l.borrow_mut().clear());
    let a = writer_chunk("");
    let (r, callbacks) = outcome(&reader_entry(), &[("a.lua", &a)]);
    r.unwrap();
    assert_eq!(callbacks, [Disposition::Lowered, Disposition::Lowered]);
    let lowered = LOWERED.with(|l| l.borrow().clone());
    assert!(lowered.iter().any(|s| s == "true"), "{lowered:?}");
}

#[test]
fn an_assignment_to_env_is_a_write() {
    // Fixture 2: the decorative `draw` rebinds the chunk's `_ENV`, the
    // binding the reader's global resolves through.
    opens(
        "",
        "function() _ENV = { skin_config = skin_config } return true end",
        true,
    );
    // Control: a local `_ENV` of its own.
    closed("", "function() local _ENV = { } return true end");
}

#[test]
fn a_capture_with_a_metatable_opens_the_candidate() {
    // C2: the `_ENV` a global is read through, and a captured table.
    opens(
        "local e = setmetatable({}, {})",
        "function() local _ENV = e return true end",
        false,
    );
    opens(
        "local t = setmetatable({}, {})",
        "function() return t.x == 1 end",
        false,
    );
    closed("local t = {}", "function() return t.x == 1 end");
}

#[test]
fn every_frame_time_write_opens_the_candidate() {
    // C3 writes: a global, a configuration entry, a table's entries or
    // metatable through the library, a host writer.
    for (pre, draw) in [
        ("", "function() g = 1 return true end"),
        ("", "function() skin_config.option.x = 2 return true end"),
        (
            "local t = {}",
            "function() table.insert(t, 1) return true end",
        ),
        ("", "function() setmetatable({}, {}) return true end"),
        ("", "function() ms.set_timer(1, 0) return true end"),
    ] {
        opens(pre, draw, true);
    }
    // Controls: a local of the same function, a table read.
    closed("", "function() local g = 1 g = 2 return true end");
    closed("local t = {}", "function() return #t > 0 end");
}

#[test]
fn a_call_outside_the_subset_opens_the_candidate() {
    let helper = "local function h() return true end";
    let replace = "local function r() return \"b\" end";
    for (pre, draw) in [
        // A callee that is no closure, reader or pure function.
        ("", "function() return f() end"),
        // Frame-time loaders.
        ("", "function() return require(\"x\") end"),
        ("", "function() return dofile(\"x.lua\") end"),
        // `gsub` with a table or a function replacement.
        ("", "function() return string.gsub(\"a\", \"a\", {}) end"),
        (
            replace,
            "function() return string.gsub(\"a\", \"a\", r) end",
        ),
        // Protected calls of an unknown target, either one of `xpcall`'s.
        ("", "function() return pcall(f) end"),
        (helper, "function() return xpcall(h, f) end"),
        (helper, "function() return xpcall(f, h) end"),
        // A function constructor at frame time.
        ("", "function() local f = function() end return true end"),
        // The strings' metatable, a method call on a string.
        ("", "function() return getmetatable(\"\") end"),
        ("", "function() return (\"x\"):sub(1) end"),
    ] {
        opens(pre, draw, false);
    }
    // Controls: a helper, protected calls of proven targets, a string
    // replacement, a format of a string.
    for (pre, draw) in [
        (helper, "function() return h() end"),
        (helper, "function() return pcall(h) end"),
        (helper, "function() return xpcall(h, h) end"),
        ("", "function() return string.gsub(\"a\", \"a\", \"b\") end"),
        ("", "function() return string.format(\"%s\", \"x\") end"),
    ] {
        closed(pre, draw);
    }
}

#[test]
fn tostring_is_outside_the_subset() {
    // The source's `tostring` consults the strings' `__tostring` first,
    // which a construction can set where the source runs it and the
    // sandbox refuses it: a decorative `tostring("x")` opens the
    // candidate, beside the critical reader of a captured option.
    let pre = "pcall(function() local m = getmetatable(\"\") if type(m) == \"table\" then \
               m.__tostring = function() skin_config.option.x = 2 return \"\" end end end)";
    opens(pre, "function() return tostring(\"x\") == \"x\" end", false);
    closed(pre, "function() return type(\"x\") == \"string\" end");
}

#[test]
fn an_operator_outside_the_subset_opens_the_candidate() {
    for (pre, draw) in [
        // Arithmetic of an unknown value and of a string.
        ("", "function(p) return p + 1 > 0 end"),
        ("", "function() return \"a\" + 1 > 0 end"),
        // `..` of a table, `#` of an unknown value.
        ("local t = {}", "function() return t .. \"x\" end"),
        ("", "function(u) return #u > 0 end"),
        // `==` of two unknown values.
        ("", "function(u, v) return u == v end"),
        // `string.byte` may give nothing: its first result, kept in a
        // local, may be nil, which `math.floor` does not take.
        (
            "local s = \"a\"",
            "function() local b = string.byte(s) return math.floor(b) > 0 end",
        ),
        // A bitwise operator, which the source's grammar does not have.
        ("", "function() return 1 & 1 == 1 end"),
    ] {
        opens(pre, draw, false);
    }
    // Controls: a local assigned again and only returned; Rm's string
    // composition of host texts; host readers in arithmetic.
    for (pre, draw) in [
        ("", "function() local a = 1 a = 2 return a end"),
        ("", "function(u) return u == 1 end"),
        (
            "",
            "function() local tx1 = ms.text(1) local tx2 = ms.text(2) return tx1 .. \" > \" .. tx2 end",
        ),
        ("", "function() return ms.event_index(55) == 2 end"),
        ("", "function() return math.floor(ms.number(1) / 2) > 0 end"),
    ] {
        closed(pre, draw);
    }
}

#[test]
fn an_iterator_outside_the_subset_opens_the_candidate() {
    for draw in [
        "function(p) for _ in tostring, p do end return true end",
        "function(u) for _ in pairs(u) do end return true end",
    ] {
        opens("", draw, false);
    }
    closed(
        "",
        "function() for _, v in ipairs({ 1 }) do end return true end",
    );
}

#[test]
fn a_statement_outside_the_subset_opens_the_candidate() {
    for draw in [
        "function() local x <const> = 1 return true end",
        "function() local function f() end return true end",
    ] {
        opens("", draw, false);
    }
    closed(
        "",
        "function() local x = 1 do local y = x end return true end",
    );
}

#[test]
fn a_root_that_is_no_skin_closure_opens_the_candidate() {
    // C4: a library function at a `draw` has no tree to walk; the reader
    // refuses at its converted value.
    let entry = skin(
        "skin_config.option.x = 1",
        &format!("{}{}", image("type"), gauge(READER)),
    );
    match outcome(&entry, &[]).0 {
        Err(ImportError::Lua {
            why:
                LuaWhy::Critical {
                    why: Unsupported::Interference,
                    ..
                },
            at: Some(at),
        }) => assert_eq!((at.file.as_str(), at.line), ("s.luaskin", 0)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn writers_never_lower() {
    // GT's score-graph latch: a helper that sets an upvalue, called by a
    // decorative `draw`.
    let latch = "local max_timer = 0\n\
                 local function latch() max_timer = ms.timer(1) return true end";
    let (r, callbacks) = outcome(&skin(latch, &image("function() return latch() end")), &[]);
    r.unwrap();
    assert_eq!(callbacks, [Disposition::Replaced(Unsupported::Writes)]);
    // Rm's target writer: a value that writes the upvalue it shows, a
    // critical EX score read.
    let (r, callbacks) = outcome(
        &number(
            "local _number = 0",
            "function() _number = ms.number(101) return _number end",
        ),
        &[],
    );
    match r {
        Err(ImportError::Lua {
            why:
                LuaWhy::Critical {
                    field: "value",
                    why: Unsupported::Writes,
                },
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(callbacks, [Disposition::Refused]);
}

/// A number drawing `value`, with `pre` before it.
fn number(pre: &str, value: &str) -> String {
    skin(
        &format!(
            "{pre}\nskin.value = {{ {{ id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4,\n\
               value = {value} }} }}"
        ),
        "{ id = \"v\", dst = { { x = 0, y = 0, w = 6, h = 6 } } },",
    )
}

#[test]
fn a_value_s_role_covers_its_whole_tree() {
    let refused = |entry: &str| match outcome(entry, &[]).0 {
        Err(ImportError::Lua {
            why:
                LuaWhy::Critical {
                    field: "value",
                    why,
                },
            ..
        }) => why,
        other => panic!("{entry}: {other:?}"),
    };
    // Each value ends in `#`, which evaluation refuses before any read is
    // attached: its role is the scan's. Rm's `getDummyNumber(102)`: a
    // helper reading its parameter's id, joined over its call sites, here
    // through a second helper; 101 is the EX score.
    let helper = "local function dummy(id) if id == 0 then return 0 end return ms.number(id) end\n\
                  local function shown(id) return dummy(id) end";
    assert_eq!(
        refused(&number(
            helper,
            "function() return dummy(110) + shown(101) + #\"\" end"
        )),
        Unsupported::Construct("#")
    );
    // A local initialized by a numeral, read inside an `if`.
    assert_eq!(
        refused(&number(
            "",
            "function() local id = 101 if id > 0 then return ms.number(id) + #\"\" end return 0 end"
        )),
        Unsupported::Construct("#")
    );
    // An unknown id: the role is unknown, so critical.
    assert_eq!(
        refused(&number("", "function(p) return ms.number(p) end")),
        Unsupported::Unresolved("id")
    );
    // An unresolved callee likewise; the call of nil also opens the
    // candidate, so the capture it reads through is void first.
    assert_eq!(
        refused(&number("local t = { }", "function() return t.f(1) end")),
        Unsupported::Interference
    );
    // Controls: decorative ids, through a helper's parameter and a local,
    // are replaced.
    for (pre, value) in [
        (helper, "function() return shown(110) + #\"\" end"),
        (
            "",
            "function() local id = 110 if id > 0 then return ms.number(id) + #\"\" end return 0 end",
        ),
    ] {
        let (r, callbacks) = outcome(&number(pre, value), &[]);
        r.unwrap_or_else(|e| panic!("{value}: {e:?}"));
        assert!(
            matches!(callbacks.as_slice(), [Disposition::Replaced(_)]),
            "{value}: {callbacks:?}"
        );
    }
}

/// Two decorative images drawn under literal callbacks, each lowered:
/// the destinations' line is 10.
fn two_lowerings() -> String {
    skin(
        "",
        &format!(
            "{}{}",
            image("function() return true end"),
            image("function() return false end")
        ),
    )
}

/// One decorative image drawn under a literal callback, lowered.
fn one_lowering() -> String {
    skin("", &image("function() return true end"))
}

/// The analysis time refusal of `entry`, with its line when located.
#[track_caller]
fn too_long(entry: &str) -> Option<u32> {
    match outcome(entry, &[]).0 {
        Err(ImportError::Lua {
            why: LuaWhy::Limit(super::LuaLimit::AnalysisTime),
            at,
        }) => at.map(|a| a.line),
        other => panic!("{other:?}"),
    }
}

/// Past [`super::limits::MAX_ANALYSIS_TIME`].
const PAST: std::time::Duration = std::time::Duration::from_secs(6);
/// Under it, twice past it in sum.
const HALF: std::time::Duration = std::time::Duration::from_secs(3);

#[test]
fn time_past_the_budget_inside_the_walk_refuses_at_its_root() {
    use super::clock::fake;
    // Test 1: at a root of the walk, located at that root's definition.
    fake::at_root(|| fake::advance(PAST));
    assert_eq!(too_long(&one_lowering()), Some(10));
    // Test 2: at a phase's start, which no callable retains.
    fake::at_phase(super::clock::Phase::Walk, true, || fake::advance(PAST));
    assert_eq!(too_long(&one_lowering()), None);
}

#[test]
fn time_between_phases_does_not_count() {
    use super::clock::{Phase, fake};
    // Test 3: past the budget only between the two lowerings.
    super::tests::at_expansion(|| fake::at_phase(Phase::Lower, false, || fake::advance(PAST)));
    let (r, callbacks) = outcome(&two_lowerings(), &[]);
    r.unwrap();
    assert_eq!(callbacks, [Disposition::Lowered, Disposition::Lowered]);
    // Test 4: never past it.
    fake::at_phase(Phase::Walk, true, || fake::advance(HALF));
    outcome(&two_lowerings(), &[]).0.unwrap();
}

#[test]
fn the_last_phase_s_time_is_checked_when_it_ends() {
    use super::clock::{Phase, fake};
    // Test 5: inside the last lowering, which no check follows but its
    // own end: refused there, at that callable.
    super::tests::at_expansion(|| fake::advance(PAST));
    assert_eq!(too_long(&one_lowering()), Some(10));
    // Test 6: two phases each under the budget, past it in sum: refused
    // where the second ends.
    fake::at_phase(Phase::Walk, true, || fake::advance(HALF));
    super::tests::at_expansion(|| fake::advance(HALF));
    assert_eq!(too_long(&one_lowering()), Some(10));
}

#[test]
fn a_parameter_assigned_again_takes_no_call_site_id() {
    // The helper's parameter is the EX score's id when it reads it, not
    // the decorative id its caller passes (`#` keeps evaluation from
    // attaching the read, so the scan decides).
    let pre = "local function shown(id)\n  id = 101\n  return ms.number(id)\nend";
    match outcome(
        &number(pre, "function() return shown(110) + #\"\" end"),
        &[],
    )
    .0
    {
        Err(ImportError::Lua {
            why: LuaWhy::Critical { field: "value", .. },
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
}

/// The analysis work of importing `entry`.
fn work(entry: &str) -> u64 {
    super::callback_tests::run_with(entry, &[], &super::Cancel::new())
        .unwrap()
        .usage
        .analysis_work
}

#[test]
fn a_protected_format_s_bytes_are_charged() {
    // The walk reads the format a protected `string.format` is given,
    // each byte charged first, as an unprotected one's.
    let call = |n: usize| {
        let fmt = "x".repeat(n);
        skin(
            "",
            &image(&format!(
                "function() return pcall(string.format, \"{fmt}%d\", 1) end"
            )),
        )
    };
    let (short, long) = (work(&call(0)), work(&call(60_000)));
    assert!(long - short >= 60_000, "{short} {long}");
}

#[test]
fn every_name_a_statement_lists_is_charged() {
    // 100 local names and 100 assignment targets, against one of each:
    // the walk visits the names and the targets in their statements and
    // the targets again in its reassignment pass, and the role scan of the
    // replaced callback each statement's lists, each extra item charged
    // five times.
    let names = |n: usize| {
        let list = (0..n)
            .map(|k| format!("a{k}"))
            .collect::<Vec<_>>()
            .join(", ");
        skin(
            "",
            &image(&format!(
                "function() local {list} {list} = 1 return true end"
            )),
        )
    };
    let (one, many) = (work(&names(1)), work(&names(100)));
    assert!(many - one >= 5 * 99, "{one} {many}");
}

#[test]
fn an_image_set_s_role_scan_is_timed() {
    use super::clock::{Phase, fake};
    // Past the budget inside the role scan of an image set's value: the
    // candidate refuses at that value, defined on line 4.
    fake::at_phase(Phase::Scan, true, || fake::advance(PAST));
    let entry = skin(
        "skin.imageset = { { id = \"s\", ref = 10, images = { \"i\", \"i\" }, value = function() return 1 end } }",
        "{ id = \"s\", dst = { { w = 64, h = 64 } } },",
    );
    assert_eq!(too_long(&entry), Some(4));
}

#[test]
fn a_protected_writer_writes() {
    // A host writer and a library writer behind `pcall`.
    opens(
        "",
        "function() pcall(ms.set_timer, 1, 0) return true end",
        true,
    );
    opens(
        "",
        "function() pcall(setmetatable, {}, {}) return true end",
        true,
    );
}

#[test]
fn a_counting_timer_is_no_slot() {
    // A decorative destination's `timer` that counts a chunk local: the
    // walk reads the written cell as unknown, so its arithmetic opens the
    // candidate, and no slot is granted in an open one.
    let entry = skin(
        "local n = 0",
        "{ id = \"i\", timer = function() n = n + 1 return 0 end, dst = { { w = 64, h = 10 } } },",
    );
    let (r, callbacks) = outcome(&entry, &[]);
    r.unwrap();
    assert_eq!(
        callbacks,
        [Disposition::Replaced(Unsupported::Construct(
            "a slot in a candidate that is not closed"
        ))]
    );
}
