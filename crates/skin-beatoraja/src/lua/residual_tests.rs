//! Partial evaluation into the expressions ([`super::residual`]) through
//! the production path: the two passes, admission and the consumer that
//! lowers each callback, with the crate's witnesses (the lowered sources,
//! each charge site's units and work, and a forced failure at a site),
//! and a Lua 5.4 oracle that runs each callback as the source calls it,
//! its result converted as the consumer converts it.

use std::collections::HashMap;

use skin_model::{ErrorKind, PLAY, Scope, State, Ty, compile};

use super::callback_tests::{folder, image, skin};
use super::residual::LIMITS;
use super::root_tests::{analysis, configured};
use super::tests::{fail_next, site};
use super::{Cancel, Disposition, LuaLimit, LuaWhy, Unsupported, read_lua_header};
use crate::error::ImportError;
use crate::import::Config;
use crate::import::CriticalPart;
use crate::import::callback::{LOWERED, LOWERED_META, TRIED};

/// What importing a skin gave: its refusal, each callable's disposition,
/// the sources lowered with their guards, parts and deviations, and the
/// report's notes when it imported.
struct Run {
    r: Result<(), ImportError>,
    callbacks: Vec<Disposition>,
    lowered: Vec<String>,
    meta: Vec<(Vec<&'static str>, Option<CriticalPart>, bool)>,
    notes: Vec<crate::import::NoteKind>,
}

/// Imports `entry` with the configuration `config` makes from its header.
fn run_with(entry: &str, config: impl FnOnce(&crate::header::Header) -> Config) -> Run {
    let d = folder(entry);
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let facts = super::facts::ImportFacts::unavailable(skin_model::ScreenKind::Play);
    let config = config(h.header());
    LOWERED.with(|l| l.borrow_mut().clear());
    LOWERED_META.with(|l| l.borrow_mut().clear());
    let o = h.complete(&config, &facts, &cancel);
    let (r, notes) = match o.result {
        Ok(i) => (Ok(()), i.report.notes.iter().map(|n| n.kind).collect()),
        Err(super::ledger::Stop::Fault(e)) => (Err(e), Vec::new()),
        Err(super::ledger::Stop::Pending(p)) => panic!("{p:?}"),
    };
    Run {
        r,
        callbacks: o.callbacks,
        lowered: LOWERED.with(|l| l.borrow().clone()),
        meta: LOWERED_META.with(|l| l.borrow().clone()),
        notes,
    }
}

/// [`run_with`] in the default configuration.
fn run(entry: &str) -> Run {
    run_with(entry, Config::defaults)
}

/// Whether admission found `entry`'s candidate closed.
fn closed(entry: &str) -> bool {
    let def = configured(entry);
    super::callback::admit(&def.arena).unwrap();
    analysis(&def).open.get().is_none()
}

/// A number drawing `value`, after `pre`.
fn number(pre: &str, value: &str) -> String {
    skin(
        &format!(
            "{pre}\nskin.value = {{ {{ id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 9,\n\
               value = {value} }} }}"
        ),
        "{ id = \"v\", dst = { { x = 0, y = 0, w = 6, h = 6 } } },",
    )
}

/// Numbers drawing each of `values`, in order, after `pre`.
fn numbers(pre: &str, values: &[&str]) -> String {
    let defs: String = values
        .iter()
        .enumerate()
        .map(|(k, v)| {
            format!(
                "{{ id = \"v{k}\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 9, value = {v} }},\n"
            )
        })
        .collect();
    let dst: String = (0..values.len())
        .map(|k| format!("{{ id = \"v{k}\", dst = {{ {{ x = {k}, y = 0, w = 6, h = 6 }} }} }},"))
        .collect();
    skin(&format!("{pre}\nskin.value = {{ {defs} }}"), &dst)
}

/// Images drawn under each of `draws`, in order.
fn images(draws: &[&str]) -> String {
    draws.iter().map(|d| image(d)).collect()
}

/// The reads the oracle and the lowered expressions are given: numbers
/// by id with their model fields, options by id with theirs, timers by id
/// with theirs.
const NUMBERS: [(i64, &str); 4] = [
    (101, "ex_score"),
    (106, "notes"),
    (110, "count.perfect"),
    (111, "count.great"),
];
const OPTIONS: [(i64, &str); 2] = [(271, "cover.enabled"), (272, "lift.enabled")];
const TIMERS: [(i64, &str); 2] = [(41, "play"), (143, "last_note")];

/// One frame's reads.
#[derive(Clone, Debug, Default)]
struct Reads {
    numbers: HashMap<i64, i64>,
    options: HashMap<i64, bool>,
    /// The timers on.
    on: Vec<i64>,
}

/// The oracle: the skin's configured construction replayed in a plain Lua
/// 5.4 VM with test host providers, its callbacks called as the source
/// calls them and converted as their consumer converts them.
struct Oracle {
    lua: mlua::Lua,
    draws: Vec<mlua::Function>,
    values: Vec<mlua::Function>,
}

impl Oracle {
    /// `entry` with `skin_config.option` holding `options`.
    fn new(entry: &str, options: &[(&str, i64)]) -> Self {
        let lua = mlua::Lua::new();
        let setup = "values, options, timers, kind = {}, {}, {}, 0\n\
            local ms = { timer_off_value = math.mininteger }\n\
            function ms.number(id) return values[id] or 0 end\n\
            function ms.option(id) return options[id] == true end\n\
            function ms.timer(id) return timers[id] or math.mininteger end\n\
            function ms.gauge_type() return kind end\n\
            skin_config = { option = {}, offset = {} }\n\
            local plain = require\n\
            require = function(n) if n == \"main_state\" then return ms end return plain(n) end";
        lua.load(setup).exec().unwrap();
        let cfg: mlua::Table = lua.globals().get("skin_config").unwrap();
        let option: mlua::Table = cfg.get("option").unwrap();
        for &(k, v) in options {
            option.set(k, v).unwrap();
        }
        let skin: mlua::Table = lua.load(entry).eval().unwrap();
        let mut draws = Vec::new();
        let dst: mlua::Table = skin.get("destination").unwrap();
        for d in dst.sequence_values::<mlua::Table>() {
            if let Ok(f) = d.unwrap().get::<mlua::Function>("draw") {
                draws.push(f);
            }
        }
        let mut values = Vec::new();
        if let Ok(t) = skin.get::<mlua::Table>("value") {
            for v in t.sequence_values::<mlua::Table>() {
                values.push(v.unwrap().get("value").unwrap());
            }
        }
        Self { lua, draws, values }
    }

    fn set(&self, reads: &Reads) {
        let g = self.lua.globals();
        let values: mlua::Table = g.get("values").unwrap();
        let options: mlua::Table = g.get("options").unwrap();
        let timers: mlua::Table = g.get("timers").unwrap();
        values.clear().unwrap();
        options.clear().unwrap();
        timers.clear().unwrap();
        for (&k, &v) in &reads.numbers {
            values.set(k, v).unwrap();
        }
        for (&k, &v) in &reads.options {
            options.set(k, v).unwrap();
        }
        for &t in &reads.on {
            timers.set(t, 1_000).unwrap();
        }
    }

    /// Each `draw` as Lua's truthiness.
    fn draws(&self, reads: &Reads) -> Vec<bool> {
        self.set(reads);
        self.draws
            .iter()
            .map(|f| {
                !matches!(
                    f.call::<mlua::Value>(()).unwrap(),
                    mlua::Value::Nil | mlua::Value::Boolean(false)
                )
            })
            .collect()
    }

    /// Each `value` as the source's integer: nil and booleans 0.
    fn values(&self, reads: &Reads) -> Vec<i64> {
        self.set(reads);
        self.values
            .iter()
            .map(|f| match f.call::<mlua::Value>(()).unwrap() {
                mlua::Value::Nil | mlua::Value::Boolean(_) => 0,
                mlua::Value::Integer(n) => n,
                other => panic!("{other:?}"),
            })
            .collect()
    }
}

/// The model state the same reads give.
fn state(reads: &Reads) -> (Vec<i32>, Vec<i32>) {
    let mut fields = vec![0_i32; PLAY.fields.len()];
    for (id, name) in NUMBERS {
        let (slot, _) = PLAY.field(name).unwrap();
        fields[usize::from(slot)] =
            i32::try_from(reads.numbers.get(&id).copied().unwrap_or(0)).unwrap();
    }
    for (id, name) in OPTIONS {
        let (slot, _) = PLAY.field(name).unwrap();
        fields[usize::from(slot)] = i32::from(reads.options.get(&id).copied().unwrap_or(false));
    }
    let mut timers = vec![skin_model::TIMER_OFF; PLAY.timers.len()];
    for (id, name) in TIMERS {
        let (slot, _) = PLAY.timer(name).unwrap();
        if reads.on.contains(&id) {
            timers[usize::from(slot)] = 1;
        }
    }
    (fields, timers)
}

/// Lowered sources `srcs` compiled as `ty`.
fn programs(srcs: &[String], ty: Ty) -> Vec<skin_model::Program> {
    srcs.iter()
        .map(|s| compile(s, &Scope::new(&PLAY), ty).unwrap_or_else(|e| panic!("{s}: {e:?}")))
        .collect()
}

/// Programs `ps` of type `ty` evaluated with `reads`.
fn evaluate(ps: &[skin_model::Program], ty: Ty, reads: &Reads) -> Vec<i64> {
    let (fields, timers) = state(reads);
    let st = State {
        fields: &fields,
        timers: &timers,
    };
    ps.iter()
        .map(|p| match ty {
            Ty::Bool => i64::from(p.eval_bool(&st)),
            _ => i64::from(p.eval(&st)),
        })
        .collect()
}

/// Every combination of the counts `counts` takes, of options 271 and
/// 272, and of the two timers on or off.
fn grid(counts: &[i64]) -> Vec<Reads> {
    let mut out = Vec::new();
    for &ex in counts {
        for &pg in counts {
            for o in 0..4 {
                for t in 0..4 {
                    let mut r = Reads::default();
                    r.numbers.insert(101, ex);
                    r.numbers.insert(110, pg);
                    r.numbers.insert(111, (ex + pg) % 7);
                    r.numbers.insert(106, ex + pg + 3);
                    r.options.insert(271, o & 1 != 0);
                    r.options.insert(272, o & 2 != 0);
                    if t & 1 != 0 {
                        r.on.push(41);
                    }
                    if t & 2 != 0 {
                        r.on.push(143);
                    }
                    out.push(r);
                }
            }
        }
    }
    out
}

/// Checks that each lowered source agrees with the oracle over `grid`:
/// the draws when `draws`, else the values.
#[track_caller]
fn agrees(entry: &str, options: &[(&str, i64)], lowered: &[String], draws: bool, counts: &[i64]) {
    let oracle = Oracle::new(entry, options);
    let ty = if draws { Ty::Bool } else { Ty::Int };
    let ps = programs(lowered, ty);
    for reads in grid(counts) {
        if draws {
            let want: Vec<i64> = oracle.draws(&reads).into_iter().map(i64::from).collect();
            assert_eq!(evaluate(&ps, ty, &reads), want, "{reads:?}");
        } else {
            assert_eq!(
                evaluate(&ps, ty, &reads),
                oracle.values(&reads),
                "{reads:?} {lowered:?}"
            );
        }
    }
}

const MAX: i64 = skin_model::MAX_COUNT as i64;

/// Rm's helper shape, written fresh: 0 until the EX score is 1, else the
/// number its argument names.
const DUMMY: &str =
    "local function dummy(n) if ms.number(101) < 1 then return 0 else return ms.number(n) end end";

#[test]
fn a_helper_with_known_arguments_lowers_as_a_conditional() {
    for arg in [110, 101] {
        let entry = number(DUMMY, &format!("function() return dummy({arg}) end"));
        assert!(closed(&entry));
        fail_next(None);
        let r = run(&entry);
        r.r.unwrap();
        assert_eq!(r.callbacks, [Disposition::Lowered]);
        let [src] = r.lowered.as_slice() else {
            panic!("{:?}", r.lowered)
        };
        assert!(
            src.starts_with("(if ((ex_score) < 1) then 0 else "),
            "{src}"
        );
        // Critical: the EX score it reads.
        assert_eq!(r.meta[0].1, Some(CriticalPart::ExScore));
        agrees(&entry, &[], &r.lowered, false, &[0, 1, 2, MAX]);
        // One helper call inlined: the inline stack scanned at one entry
        // and one parameter bound (the root's empty stack and none).
        assert_eq!(site("scan").0, 1);
        assert_eq!(site("bind").0, 1);
    }
}

#[test]
fn r4_conditionals_need_all_four_proofs() {
    use Disposition::{Lowered, Replaced};
    // int-or-bool: a string value is no type a number takes; a raw timer
    // value is an integer of no known bound (its start or the off value),
    // which a number's value may not convert.
    for (value, why) in [
        ("function() return ms.text(1) end", Unsupported::Type),
        ("function() return ms.timer(41) end", Unsupported::Range),
    ] {
        let entry = number("", value);
        assert!(closed(&entry), "{value}");
        assert_eq!(run(&entry).callbacks, [Replaced(why)], "{value}");
    }
    // total: a definite Lua error on a reached path; control: behind a
    // condition folded false.
    let r = run(&number("", "function() local x = nil return x + 1 end"));
    assert_eq!(
        r.callbacks,
        [Replaced(Unsupported::Construct("a Lua error"))]
    );
    let r = run(&number(
        "",
        "function() if false then return nil + 1 end return 0 end",
    ));
    assert_eq!(
        (r.callbacks, r.lowered),
        (vec![Lowered], vec!["0".to_owned()])
    );
    // pure: a library call, which only the program takes.
    let entry = number("", "function() return math.max(ms.number(110), 0) end");
    assert!(closed(&entry));
    assert_eq!(run(&entry).callbacks, [Disposition::Compiled]);
    // range: an arm whose interval leaves `i32`.
    let (_, notes) = PLAY.field("notes").unwrap();
    assert!(
        matches!(notes.ty, skin_model::FieldTy::Int { hi, .. } if i64::from(hi) * i64::from(hi) > i64::from(i32::MAX))
    );
    let entry = number(
        "",
        "function() if ms.option(271) then return ms.number(106) * ms.number(106) end return 0 end",
    );
    assert!(closed(&entry));
    // Past `i32` but within ±2^53: the program's, whose integers are Lua's.
    assert_eq!(run(&entry).callbacks, [Disposition::Compiled]);
    // range: 17 nested integer conditionals, within the bytes, the ops
    // and the nesting, past R4's stack.
    let arms: String = (0..17)
        .map(|k| {
            let cond = match k % 3 {
                0 => format!("ms.option({})", 270 + k % 4),
                1 => format!("ms.number(110) > {k}"),
                _ => format!("ms.option({}) and ms.number(111) < {k}", 270 + k % 4),
            };
            let kw = if k == 0 { "if" } else { "elseif" };
            format!(
                "{kw} {cond} then return ms.number({}) ",
                if k % 2 == 0 { 110 } else { 111 }
            )
        })
        .collect();
    let value = format!("function() {arms}else return 0 end end");
    let entry = number("", &value);
    assert!(closed(&entry));
    TRIED.with(|t| *t.borrow_mut() = None);
    // Past R4's stack: the program takes it, a register a level.
    assert_eq!(run(&entry).callbacks, [Disposition::Compiled]);
    let (src, kind) = TRIED.with(|t| t.borrow().clone()).unwrap();
    assert!(src.len() < skin_model::MAX_SOURCE_BYTES, "{}", src.len());
    let ops = src
        .matches(['(', '+', '-', '*', '<', '>', '=', '!', '&', '|'])
        .count();
    assert!(ops < skin_model::MAX_OPS, "{ops}");
    let mut depth = 0_usize;
    let mut deepest = 0;
    for c in src.chars() {
        match c {
            '(' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            ')' => depth -= 1,
            _ => {}
        }
    }
    assert!(deepest < skin_model::MAX_NESTING, "{deepest}");
    assert!(
        matches!(kind, Some(ErrorKind::StackTooDeep { .. })),
        "{kind:?}"
    );
    // Critical, the same shape is compiled too.
    let critical = number(
        "",
        &value.replace("return 0 end end", "return ms.number(101) end end"),
    );
    let r = run(&critical);
    assert!(r.r.is_ok(), "{:?}", r.r);
    assert_eq!(r.callbacks, [Disposition::Compiled]);
}

#[test]
fn and_or_keep_lua_values() {
    let values = [
        "function() return ms.number(110) or 5 end",
        "function() return ms.option(271) and 7 end",
        "function() return true end",
    ];
    let entry = numbers("", &values);
    assert!(closed(&entry));
    let r = run(&entry);
    assert_eq!(r.callbacks, [Disposition::Lowered; 3]);
    assert_eq!(r.lowered[2], "0");
    agrees(&entry, &[], &r.lowered, false, &[0, 1, MAX]);
    let draws = [
        "function() return 0 and ms.option(271) end",
        "function() return \"\" and ms.option(271) end",
        "function() if ms.option(271) then return true end end",
    ];
    let entry = skin("", &images(&draws));
    assert!(closed(&entry));
    let r = run(&entry);
    assert_eq!(r.callbacks, [Disposition::Lowered; 3]);
    agrees(&entry, &[], &r.lowered, true, &[0, 1]);
}

/// A play skin with an option `x` (items 905 and 906, 905 the default).
fn optioned(body: &str, dst: &str) -> String {
    skin(body, dst).replacen(
        "h = 1080 }",
        "h = 1080, property = { { name = \"x\", item = { { name = \"a\", op = 905 }, { name = \"b\", op = 906 } } } } }",
        1,
    )
}

#[test]
fn configuration_integers_lower() {
    let value = "skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 9,\n\
        value = function() return skin_config.option[\"x\"] * 10 + ms.number(110) end } }";
    let entry = optioned(
        value,
        "{ id = \"v\", dst = { { x = 0, y = 0, w = 6, h = 6 } } },",
    );
    assert!(closed(&entry));
    for x in [905, 906] {
        let r = run_with(&entry, |_| Config {
            options: vec![Some(x)],
            ..Config::default()
        });
        assert_eq!(r.callbacks, [Disposition::Lowered], "{x}");
        assert_eq!(r.lowered, [format!("({} + (count.perfect))", x * 10)]);
        agrees(
            &entry,
            &[("x", i64::from(x))],
            &r.lowered,
            false,
            &[0, 1, MAX],
        );
    }
}

#[test]
fn timers_compare_with_a_captured_off_value() {
    let entry = skin(
        "local OFF = ms.timer_off_value",
        &image("function() return ms.timer(41) == OFF or ms.timer(143) ~= OFF end"),
    );
    assert!(closed(&entry));
    // The marker: one declaration recorded at parse, its bytes rewritten,
    // its record copied.
    fail_next(None);
    let def = configured(&entry);
    let an = analysis(&def);
    let [(key, true)] = an.marks.iter().map(|(&k, &v)| (k, v)).collect::<Vec<_>>()[..] else {
        panic!("{:?}", an.marks)
    };
    assert_eq!(site("mark"), (1, 1));
    let bytes = format!("tapestry_tag0({key},").len() + ",nil,true)".len();
    assert_eq!(site("mark bytes"), (u64::try_from(bytes).unwrap(), 1));
    assert_eq!(site("mark copy"), (1, 1));
    let r = run(&entry);
    assert_eq!(r.callbacks, [Disposition::Lowered]);
    assert_eq!(r.lowered, ["((!on(play)) || on(last_note))"]);
    agrees(&entry, &[], &r.lowered, true, &[0]);
    // Control: an integer that is no off value is a timer's value.
    let entry = skin(
        "local K = 5",
        &image("function() return ms.timer(41) == K end"),
    );
    assert_eq!(
        run(&entry).callbacks,
        [Disposition::Replaced(Unsupported::Type)]
    );
}

#[test]
fn host_arguments_fold_from_upvalues() {
    for (op, want) in [
        (271, Disposition::Lowered),
        (
            1_177,
            Disposition::Replaced(Unsupported::Unmapped { api: 0, id: 1_177 }),
        ),
        (
            2_246,
            Disposition::Replaced(Unsupported::Unmapped { api: 0, id: 2_246 }),
        ),
    ] {
        let entry = skin(
            &format!("local op = {op}"),
            &image("function() return ms.option(op) end"),
        );
        assert!(closed(&entry));
        let r = run(&entry);
        assert_eq!(r.callbacks, [want], "{op}");
        if op == 271 {
            assert_eq!(r.lowered, ["(cover.enabled)"]);
        }
    }
}

#[test]
fn branches_merge_locals_and_returns() {
    let values = [
        "function() local x = 1 if ms.option(271) then x = 2 end return x end",
        "function() if ms.option(271) then return 1 end return 2 end",
        "function() if ms.option(271) then if ms.option(272) then return 1 end end return 2 end",
        "function() local a, b = 1, 2 if ms.option(271) then a, b = b, a end return a end",
        "function() local a = 1 do local a = 2 end return a end",
        "function() local a, a = 1, 2 return a end",
        "function() if ms.option(271) then if ms.option(272) then return 1 end else if ms.option(272) then return 2 end end return 3 end",
    ];
    let entry = numbers("", &values);
    assert!(closed(&entry));
    let r = run(&entry);
    assert_eq!(r.callbacks, [Disposition::Lowered; 7]);
    assert_eq!(r.lowered[0], "(if (cover.enabled) then 2 else 1)");
    assert_eq!(
        r.lowered[2],
        "(if ((cover.enabled) && (lift.enabled)) then 1 else 2)"
    );
    assert_eq!((r.lowered[4].as_str(), r.lowered[5].as_str()), ("1", "2"));
    agrees(&entry, &[], &r.lowered, false, &[0]);
    // A target assigned twice follows the role.
    let twice = "function() local a = 0 a, a = 1, 2 return a end";
    let why = Unsupported::Construct("a target assigned twice");
    assert_eq!(
        run(&number("", twice)).callbacks,
        [Disposition::Replaced(why)]
    );
    let critical =
        "function() local a = 0 a, a = 1, 2 if a == 1 then return ms.number(101) end return 0 end";
    match run(&number("", critical)).r {
        Err(ImportError::Lua {
            why: LuaWhy::Critical { why: w, .. },
            ..
        }) => assert_eq!(w, why),
        other => panic!("{other:?}"),
    }
}

/// A frame's `number(96)` reads the chart's play level under its validity,
/// through the same mapping as a `ref`, and no fact: nothing makes the skin
/// pending. What could leave `i32` stays unsupported.
#[test]
fn a_frames_play_level_read_lowers_under_its_validity() {
    let r = run(&number("", "function() return ms.number(96) end"));
    assert_eq!(r.callbacks, [Disposition::Lowered]);
    assert_eq!(r.lowered, ["(chart.level)"]);
    assert_eq!(r.meta[0].0, ["chart.level_valid"]);
    assert!(!r.meta[0].2);
    let r = run(&skin(
        "",
        &image("function() return ms.number(96) >= 12 end"),
    ));
    assert_eq!(r.callbacks, [Disposition::Lowered]);
    assert_eq!(r.lowered, ["chart.level_valid && ((chart.level) >= 12)"]);
    // What could leave `i32` is the program's, whose integers are Lua's.
    let r = run(&number("", "function() return ms.number(96) + 2 end"));
    assert_eq!(r.callbacks, [Disposition::Compiled]);
}

#[test]
fn conversion_keeps_its_reads() {
    // A BPM read a draw converts to true: its guard and D4 kept.
    let r = run(&skin("", &image("function() return ms.number(160) end")));
    assert_eq!(r.callbacks, [Disposition::Lowered]);
    assert_eq!(r.lowered, ["bpm.current_valid && true"]);
    assert!(r.meta[0].2);
    assert!(r.notes.contains(&crate::import::NoteKind::Deviation(
        crate::import::DeviationId::D4
    )));
    // An unmapped read a conversion or a fold drops is still refused.
    let unmapped = Disposition::Replaced(Unsupported::Unmapped { api: 1, id: 426 });
    for draw in [
        "function() return ms.number(426) end",
        "function() return ms.number(426) == nil end",
    ] {
        assert_eq!(run(&skin("", &image(draw))).callbacks, [unmapped], "{draw}");
    }
    // A read in either arm joins its guard.
    let r = run(&number(
        "",
        "function() if ms.option(271) then return 1 else return ms.number(160) end end",
    ));
    assert_eq!(r.callbacks, [Disposition::Lowered]);
    assert_eq!(r.meta[0].0, ["bpm.current_valid"]);
}

#[test]
fn integers_follow_the_source_s_double_model() {
    use Disposition::{Lowered, Replaced};
    let range = Replaced(Unsupported::Range);
    // Numerals, literal-only: where LuaJ and Lua 5.4 read another value.
    for (draw, want) in [
        ("function() return 0xffffffffffffffff < 0 end", range),
        (
            "function() return 9007199254740992 == 9007199254740993 end",
            range,
        ),
        (
            "function() return 9007199254740992 == 9007199254740992 end",
            Lowered,
        ),
        // A result folded past 2^53 is the source's double: 2^53 + 1
        // rounds to 2^53, so the difference is 0; the control rounds up.
        (
            "function() return (9007199254740992 + 1) - 9007199254740992 == 0 end",
            Lowered,
        ),
        (
            "function() return (9007199254740992 + 3) - 9007199254740992 == 4 end",
            Lowered,
        ),
        // A float's floor past 2^53 stays the float, and folds.
        ("function() return math.floor(1e18) > 0 end", Lowered),
        // A host argument folded just past 2^53 is a float, which no
        // host argument is; one at it is an integer.
        (
            "function() return ms.number(4503599627370496 * 2 + 1) > 0 end",
            Replaced(Unsupported::Construct("host arguments")),
        ),
        (
            "function() return ms.number(4503599627370496 * 2) > 0 end",
            Replaced(Unsupported::Unmapped {
                api: 1,
                id: 1 << 53,
            }),
        ),
    ] {
        assert_eq!(run(&skin("", &image(draw))).callbacks, [want], "{draw}");
    }
    // The folded values are the source's: each draw's lowered source, an
    // exact result's opposite where it rounds.
    for (draw, shown) in [
        ("(9007199254740992 + 1) - 9007199254740992 == 0", "true"),
        ("(9007199254740992 + 1) - 9007199254740992 == 1", "false"),
        ("(9007199254740992 + 3) - 9007199254740992 == 4", "true"),
        ("math.floor(1e18) > 0", "true"),
        (
            "(2147483647 * 4194305) * 3 == 2.7021604194091008e16",
            "true",
        ),
        (
            "(2147483647 * 4194305) * 3 == 2.7021604194091004e16",
            "false",
        ),
    ] {
        let r = run(&skin("", &image(&format!("function() return {draw} end"))));
        assert_eq!(r.callbacks, [Lowered], "{draw}");
        assert_eq!(r.lowered, [shown], "{draw}");
    }
    // Captures past 2^53, and off values without their provenance; and
    // the controls with it.
    let wraps = "9007199254740992 * 1024";
    let timer = "function() return ms.timer(41) == K end";
    let count = "function() return ms.number(110) == K end";
    for (body, draw, want) in [
        ("local K = 9007199254740993".to_owned(), count, range),
        (format!("local K = {wraps}"), timer, range),
        ("K = 9007199254740993".to_owned(), count, range),
        (
            "local t = { k = 9007199254740993 }".to_owned(),
            "function() return ms.number(110) == t.k end",
            range,
        ),
        (
            format!("local fake = {{ timer_off_value = {wraps} }}\nlocal K = fake.timer_off_value"),
            timer,
            range,
        ),
        (
            "local K = ms.timer_off_value\nK = ms.timer_off_value".to_owned(),
            timer,
            range,
        ),
        // Under the construction premise, a value construction wrapped
        // into range folds.
        ("local K = 0xffffffffffffffff".to_owned(), count, Lowered),
        ("local K = ms.timer_off_value".to_owned(), timer, Lowered),
        (
            String::new(),
            "function() return ms.timer(41) == ms.timer_off_value end",
            Lowered,
        ),
    ] {
        let entry = skin(&body, &image(draw));
        assert!(closed(&entry), "{body}");
        assert_eq!(run(&entry).callbacks, [want], "{body} / {draw}");
    }
}

/// Folding equals evaluation past 2^53: a number's value folded from
/// literals at import gives what the program gives when it runs the same
/// operations on reads of those values, each built in the frame from
/// fields of non-default values (a read is an `i32`, so `n = 2^53` is
/// `2^27 · 2^26`). The program's side is a verified program built by hand
/// with the instructions the compiler emits. The value ends in the floor
/// of `% 2^32`, an integer the expressions hold, so the fold lowers to it;
/// each pair checks the value, the fold's disposition and the run's
/// availability, so neither side passes by giving a default 0.
#[test]
fn folding_past_2_53_equals_evaluation() {
    use skin_model::present::{
        Arith, Bank, CallbackDef, Clock, Conv, Insn, Io, MathFn, OutTy, Present, PresentDef,
        ReadDef, Val,
    };
    let field = |name: &str| PLAY.fields.iter().position(|f| f.name == name).unwrap();
    let (x, y) = (field("time.elapsed_s"), field("best.rate_ppm"));
    let read = |name: &str| ReadDef {
        value: compile(name, &Scope::new(&PLAY), Ty::Int).unwrap(),
        guard: None,
    };
    // Reads `a` and `b`, then `a * b`, then each of `ops` with its
    // constant, in order, then `math.floor`.
    let run_program = |a: i32, b: i32, ops: &[(Arith, i64)]| {
        let mut code = vec![
            Insn::Read { dst: 0, read: 0 },
            Insn::Read { dst: 1, read: 1 },
            Insn::Arith {
                op: Arith::Mul,
                dst: 0,
                a: 0,
                b: 1,
            },
        ];
        for (k, &(op, _)) in ops.iter().enumerate() {
            code.push(Insn::Const {
                dst: 1,
                k: u16::try_from(k).unwrap(),
            });
            code.push(Insn::Arith {
                op,
                dst: 0,
                a: 0,
                b: 1,
            });
        }
        code.push(Insn::Math {
            f: MathFn::Floor,
            dst: 0,
            a: 0,
        });
        code.push(Insn::Ret { src: 0 });
        let def = PresentDef {
            callbacks: vec![CallbackDef {
                start: 0,
                len: u16::try_from(code.len()).unwrap(),
                regs: 2,
                conv: Conv::Int,
                gate: None,
                after: None,
            }],
            code,
            consts: ops.iter().map(|&(_, k)| Val::Int(k)).collect(),
            reads: vec![read("time.elapsed_s"), read("best.rate_ppm")],
            signature: vec![OutTy::Int],
            slots: vec![],
            domains: vec![],
        };
        let p =
            Present::verify(def, PLAY.fields.len(), &mut skin_model::SkinOps::default()).unwrap();
        let mut fields = vec![0; PLAY.fields.len()];
        fields[x] = a;
        fields[y] = b;
        let state = State {
            fields: &fields,
            timers: &[],
        };
        let mut outs = [0; 2];
        p.run(&mut Io {
            state: &state,
            clock: Clock {
                state_us: 0,
                now: bms_model::TimeUs(0),
            },
            bank: &mut Bank::default(),
            outs: &mut outs,
            timers: &mut [],
            slots: true,
        });
        outs
    };
    let (mul, add, rem) = (Arith::Mul, Arith::Add, Arith::Mod);
    let m = 1_i64 << 32;
    for (folded, a, b, ops, want) in [
        // A: the product's tie rounds, then the product by 3 again (rounded
        // once at the end, 2 134 900 732).
        (
            "math.floor(2147483647 * 4194305 * 3 % 4294967296)",
            2_147_483_647,
            4_194_305,
            vec![(mul, 3), (rem, m)],
            2_134_900_736,
        ),
        // B: 2^53 + 1 rounds to 2^53, and + 1 again: 0 (exactly, 2).
        (
            "math.floor((134217728 * 67108864 + 1 + 1) % 4294967296)",
            134_217_728,
            67_108_864,
            vec![(add, 1), (add, 1), (rem, m)],
            0,
        ),
        // Its control: 2^53 + 2 is exact.
        (
            "math.floor((134217728 * 67108864 + 1 + 2) % 4294967296)",
            134_217_728,
            67_108_864,
            vec![(add, 1), (add, 2), (rem, m)],
            2,
        ),
    ] {
        let r = run(&number("", &format!("function() return {folded} end")));
        assert_eq!(r.callbacks, [Disposition::Lowered], "{folded}");
        assert_eq!(r.lowered, [want.to_string()], "{folded}");
        assert_eq!(run_program(a, b, &ops), [want, 1], "{folded}");
    }
}

#[test]
fn the_marker_takes_the_conjunction_over_executions() {
    use Disposition::{Lowered, Replaced};
    let range = Replaced(Unsupported::Range);
    let fake = "local fake = { timer_off_value = 9007199254740992 * 1024 }\n\
        local function mk(x) local OFF = x.timer_off_value return function() return ms.timer(41) == OFF end end";
    // The same declaration run twice; the retained instance's own read
    // was the module's or the fake's, the provenance the conjunction.
    for (made, want) in [
        ("local _ = mk(fake)\nlocal f = mk(ms)", range),
        ("local f = mk(ms)\nlocal _ = mk(fake)", range),
        ("local _ = mk(ms)\nlocal f = mk(ms)", Lowered),
    ] {
        let entry = skin(&format!("{fake}\n{made}"), &image("f"));
        assert!(closed(&entry), "{made}");
        assert_eq!(run(&entry).callbacks, [want], "{made}");
    }
    // Two shadowed declarations are independent.
    let entry = skin(
        &format!(
            "{fake}\nlocal OFF = fake.timer_off_value\nlocal g = function() return ms.timer(41) == OFF end\n\
             local h\ndo local OFF = ms.timer_off_value h = function() return ms.timer(41) == OFF end end"
        ),
        &format!("{}{}", image("g"), image("h")),
    );
    assert!(closed(&entry));
    assert_eq!(run(&entry).callbacks, [range, Lowered]);
    // `X` a global read through an `_ENV` rebound to a table holding the
    // fake, or the module.
    for (module, want) in [("fake", range), ("ms", Lowered)] {
        let entry = skin(
            &format!(
                "{fake}\nlocal _ENV = {{ main_state = {module} }}\nlocal OFF = main_state.timer_off_value"
            ),
            &image("function() return ms.timer(41) == OFF end"),
        );
        assert!(closed(&entry), "{module}");
        assert_eq!(run(&entry).callbacks, [want], "{module}");
    }
    // A fake with `__eq` is still not the module.
    let entry = skin(
        "local fake = setmetatable({ timer_off_value = 9007199254740992 * 1024 }, { __eq = function() return true end })\n\
         local OFF = fake.timer_off_value",
        &image("function() return ms.timer(41) == OFF end"),
    );
    assert!(closed(&entry));
    assert_eq!(run(&entry).callbacks, [range]);
}

#[test]
fn residual_height_and_distribution() {
    // A chain of 65 locals, each one deeper: an integer a draw converts
    // to true, so the height is checked as each node is made, before any
    // is emitted.
    let mut locals = String::from("local a1 = ms.number(110) ");
    for k in 2..=65 {
        locals.push_str(&format!("local a{k} = a{} + 1 ", k - 1));
    }
    let entry = skin("", &image(&format!("function() {locals}return a65 end")));
    assert!(closed(&entry));
    fail_next(None);
    assert_eq!(run(&entry).callbacks, [Disposition::Replaced(LIMITS)]);
    assert_eq!(site("emit"), (0, 0));
    // A mixed kind distributes into the arms.
    let entry = skin(
        "",
        &image("function() return (ms.option(271) and ms.number(110)) == 5 end"),
    );
    assert!(closed(&entry));
    fail_next(None);
    let r = run(&entry);
    assert_eq!(r.lowered, ["((cover.enabled) && ((count.perfect) == 5))"]);
    // W12: the truthiness of `and`'s operand, the equality and its two
    // arms, and the draw's conversion.
    assert_eq!(site("visit").0, 5);
}

#[test]
fn what_partial_evaluation_leaves_follows_the_role() {
    let c = Unsupported::Construct;
    for (body, draw, why) in [
        (
            "",
            "function() while false do end return true end",
            c("while"),
        ),
        (
            "",
            "function() return (\"\"):len() > 0 end",
            c("method call"),
        ),
        ("", "function() return {} end", c("table")),
        ("", "function() return \"a\" .. \"b\" end", c("..")),
        ("", "function() return #\"ab\" > 0 end", c("#")),
        (
            "",
            "function() return string.len(\"a\") > 0 end",
            c("library call"),
        ),
        (
            "local function r(n) return r(n) end",
            "function() return r(1) end",
            Unsupported::Recursion,
        ),
    ] {
        let r = run(&skin(body, &image(draw)));
        assert_eq!(r.callbacks, [Disposition::Replaced(why)], "{draw}");
    }
    // Floats, `/ // %` and `math` the expressions refuse go to the
    // program; known operands fold.
    for (draw, want) in [
        (
            "function() return ms.number(110) > 1.5 end",
            Disposition::Compiled,
        ),
        (
            "function() return math.max(ms.number(110), 2) > 0 end",
            Disposition::Compiled,
        ),
        (
            "function() return ms.number(110) / 2 > 0 end",
            Disposition::Compiled,
        ),
        (
            "function() return ms.number(110) % 2 > 0 end",
            Disposition::Compiled,
        ),
        ("function() return 3 // 2 > 0 end", Disposition::Lowered),
        (
            "function() return math.max(1, 2) > 0 end",
            Disposition::Lowered,
        ),
    ] {
        assert_eq!(run(&skin("", &image(draw))).callbacks, [want], "{draw}");
    }
}

#[test]
fn a_helper_with_arguments_is_void_while_the_analysis_is_open() {
    super::root_tests::interferes(
        &super::root_tests::opened(
            "local function h(x) return ms.number(110) > x end",
            "function() return h(1) end",
        ),
        super::root_tests::script("draw"),
    );
    // A literal-only callback still lowers.
    let entry = super::root_tests::opened("", "function() return 1 + 2 > 2 end");
    assert_eq!(run(&entry).callbacks[0], Disposition::Lowered);
}

/// The line of `entry` that lists the destinations: where a lowering's
/// fatal stop is located.
fn destinations(entry: &str) -> u32 {
    let k = entry
        .lines()
        .position(|l| l.starts_with("skin.destination"))
        .unwrap();
    u32::try_from(k + 1).unwrap()
}

/// Checks charge site `site`: `entry` makes the real charge there add
/// `units` to its production counter, and with that counter spent before
/// the site's first nonempty charge, the real check stops the import
/// fatally for `why` at `line` of `s.luaskin`, the site's work never
/// started.
#[track_caller]
fn witness(name: &'static str, entry: &str, units: u64, why: LuaWhy, line: u32) {
    fail_next(None);
    let r = run(entry);
    r.r.unwrap_or_else(|e| panic!("{name}: {e:?}"));
    assert_eq!(site(name).0, units, "{name}");
    let (r, started) = forced(name, entry, name);
    match r {
        Err(ImportError::Lua { why: w, at }) => {
            assert_eq!(w, why, "{name}");
            let at = at.unwrap();
            assert_eq!((at.file.as_str(), at.line), ("s.luaskin", line), "{name}");
        }
        other => panic!("{name}: {other:?}"),
    }
    assert_eq!(started, 0, "{name}: its work started");
}

#[test]
fn every_new_charge_is_made_before_its_work() {
    let work = LuaWhy::Limit(LuaLimit::AnalysisWork);
    let at = |entry: &str| destinations(entry);
    // W1, at the parser's phase and file.
    let entry = skin("local wide, exact = 9007199254740993, 9007199254740992", "");
    witness("wide", &entry, 1, LuaWhy::Limit(LuaLimit::AstNodes), 0);
    // W2: the marked declaration, its rewrite and its record.
    let entry = skin(
        "local OFF = ms.timer_off_value",
        &image("function() return ms.timer(41) == OFF end"),
    );
    let key = *analysis(&configured(&entry)).marks.keys().next().unwrap();
    let bytes = units(&format!("tapestry_tag0({key},,nil,true)"));
    witness("mark", &entry, 1, LuaWhy::Limit(LuaLimit::AstNodes), 0);
    witness(
        "mark bytes",
        &entry,
        bytes,
        LuaWhy::Limit(LuaLimit::Generated),
        0,
    );
    witness("mark copy", &entry, 1, LuaWhy::Limit(LuaLimit::Captures), 0);
    // W3: ten statements.
    let body: String = (0..9).map(|k| format!("local a{k} = {k} ")).collect();
    let entry = skin(
        "",
        &image(&format!("function() {body}return ms.number(110) > a8 end")),
    );
    witness("stmt", &entry, 10, work, at(&entry));
    // W4: a helper chain three deep, each level of the walk: 16
    // expression and call levels and the four bodies' blocks.
    let entry = skin(
        "local function h3() return ms.number(110) > 0 end\n\
         local function h2() return h3() end\nlocal function h1() return h2() end",
        &image("function() return h1() end"),
    );
    witness(
        "depth",
        &entry,
        20,
        LuaWhy::Limit(LuaLimit::AstDepth),
        at(&entry),
    );
    // W5: the module, the key, the host function, the argument, the
    // read, the bound, the comparison and its negation.
    let entry = skin("", &image("function() return not (ms.number(110) > 0) end"));
    witness(
        "node",
        &entry,
        8,
        LuaWhy::Limit(LuaLimit::Expansion),
        at(&entry),
    );
    // W6, by role: the height checked as each node is made. The chain of
    // 65 locals pushes its read's five nodes, two a step to `a64`, and
    // `a65`'s 1, then refuses `a65`'s sum before any is emitted.
    let mut locals = String::from("local a1 = ms.number(110) ");
    for k in 2..=65 {
        locals.push_str(&format!("local a{k} = a{} + 1 ", k - 1));
    }
    let entry = skin("", &image(&format!("function() {locals}return a65 end")));
    fail_next(None);
    let r = run(&entry);
    assert_eq!(r.callbacks, [Disposition::Replaced(LIMITS)]);
    assert_eq!(site("node").0, 5 + 63 * 2 + 1);
    assert_eq!(site("emit"), (0, 0));
    // With the nesting the real check takes lowered to 2: the first node
    // above it, the comparison, is refused, after the eight below it.
    let entry = skin("", &image("function() return ms.number(110) + 1 > 0 end"));
    fail_next(Some("height"));
    let r = run(&entry);
    let nodes = site("node").0;
    fail_next(None);
    assert_eq!(r.callbacks, [Disposition::Replaced(LIMITS)]);
    assert_eq!(nodes, 8);
    // W7: two reads.
    let entry = skin(
        "",
        &image("function() return ms.number(110) == ms.number(111) end"),
    );
    witness("read", &entry, 2, work, at(&entry));
    // W8: fifty helpers deep, each call scanning the stack before it.
    let mut chain = String::from("local function h50() return ms.number(110) > 0 end\n");
    for k in (1..50).rev() {
        chain.push_str(&format!("local function h{k}() return h{}() end\n", k + 1));
    }
    let entry = skin(&chain, &image("function() return h1() end"));
    witness("scan", &entry, (0..=50).sum(), work, at(&entry));
    // W9: two hundred parameters bound, none passed.
    let params: Vec<String> = (0..200).map(|k| format!("p{k}")).collect();
    let entry = skin(
        &format!(
            "local function many({}) return ms.number(110) > 0 end",
            params.join(", ")
        ),
        &image("function() return many() end"),
    );
    witness("bind", &entry, 200, work, at(&entry));
    // W10: five locals copied and merged at three residual `if`s.
    let entry = skin(
        "",
        &image(
            "function() local a, b, c, d, e = 1, 2, 3, 4, 5 \
             if ms.option(271) then a = 6 end if ms.option(272) then b = 7 end if ms.option(273) then c = 8 end \
             return a == 6 or b == 7 or c == 8 or d == e end",
        ),
    );
    witness("env", &entry, 30, work, at(&entry));
    // W11: a thousand-byte equality.
    let s = "a".repeat(1_000);
    let entry = skin(
        "",
        &image(&format!("function() return \"{s}\" == \"{s}\" end")),
    );
    witness("str", &entry, 1_000, work, at(&entry));
    // W12.
    let entry = skin(
        "",
        &image("function() return (ms.option(271) and ms.number(110)) == 5 end"),
    );
    witness("visit", &entry, 5, work, at(&entry));
    // W13: one read attached.
    let entry = skin("", &image("function() return ms.number(160) end"));
    witness("attach", &entry, 1, work, at(&entry));
    // W14: six nodes emitted.
    let entry = skin(
        "",
        &image("function() return not (ms.number(110) + ms.number(111) > 0) end"),
    );
    witness("emit", &entry, 6, work, at(&entry));
    // W15: the root's source copied out of the memo, a read shared twice.
    let entry = skin(
        "",
        &image("function() local a = ms.number(110) return a + a > 0 end"),
    );
    let src = "(((count.perfect) + (count.perfect)) > 0)";
    witness("memo", &entry, units(src), work, at(&entry));
    // W16 and W17: the generated bytes, and the trial compile of the
    // source.
    let entry = skin("", &image("function() return ms.option(271) end"));
    let src = "(cover.enabled)";
    witness("text", &entry, 2 * units(src), work, at(&entry));
    witness("compile", &entry, units(src), work, at(&entry));
}

fn units(s: &str) -> u64 {
    u64::try_from(s.len()).unwrap()
}

/// Imports `entry` with the charge at `site` exhausted: the stop, and how
/// often `work`, the work that charge protects, started.
fn forced(name: &'static str, entry: &str, work: &'static str) -> (Result<(), ImportError>, u64) {
    fail_next(Some(name));
    let r = run(entry);
    let started = site(work).1;
    fail_next(None);
    (r.r, started)
}

#[test]
fn an_option_s_mapping_is_charged_before_it_is_built() {
    // Its bound charged before the mapping builds its condition.
    let entry = skin("", &image("function() return ms.option(271) end"));
    let (r, started) = forced("option", &entry, "work:option");
    assert!(
        matches!(
            r,
            Err(ImportError::Lua {
                why: LuaWhy::Limit(LuaLimit::AnalysisWork),
                ..
            })
        ),
        "{r:?}"
    );
    assert_eq!(started, 0);
}

#[test]
fn an_integer_s_digits_are_charged_before_they_are_written() {
    let entry = number("", "function() return 7 end");
    let (r, started) = forced("text", &entry, "work:int");
    assert!(r.is_err(), "{r:?}");
    assert_eq!(started, 0);
}

#[test]
fn the_marker_s_bytes_are_measured_before_they_are_formatted() {
    let entry = skin(
        "local OFF = ms.timer_off_value",
        &image("function() return ms.timer(41) == OFF end"),
    );
    let (r, started) = forced("mark bytes", &entry, "work:mark");
    assert!(r.is_err(), "{r:?}");
    assert_eq!(started, 0);
}

#[test]
fn a_statement_s_lists_are_charged_before_they_are_read() {
    // `local` names and values, an assignment's targets and values, a
    // helper call's arguments and a host call's: each list's length
    // charged before it is scanned or buffered.
    let entry = skin(
        "local function h(x, y) return x == 1 end",
        &image(
            "function() local a, b = 1, 2 a, b = 3, 4 return h(1, 2) and ms.number(110) > 0 end",
        ),
    );
    assert!(closed(&entry));
    fail_next(None);
    let r = run(&entry);
    assert_eq!(r.callbacks, [Disposition::Lowered]);
    let lists = [
        ("list:local", 4),
        ("list:assign", 4),
        ("list:call", 2),
        ("list:host", 1),
    ];
    for (name, units) in lists {
        assert_eq!(site(name).0, units, "{name}");
    }
    for (name, _) in lists {
        // The real allowance spent at that list: refused there, its
        // work never started.
        let (r, started) = forced(name, &entry, name);
        match r {
            Err(ImportError::Lua {
                why: LuaWhy::Limit(LuaLimit::AnalysisWork),
                at: Some(at),
            }) => assert_eq!(at.line, destinations(&entry), "{name}"),
            other => panic!("{name}: {other:?}"),
        }
        assert_eq!(started, 0, "{name}");
    }
}

#[test]
fn a_helper_tree_inlined_by_many_consumers_meets_the_expansion_cap() {
    // A known limit: nothing is memoized across calls or lowerings, so a
    // binary tree of 16 383 helper closures, which each of 32 numbers
    // calls, is inlined in full by every consumer, past the cumulative
    // node cap: fatal.
    let entry = super::callback_tests::helper_tree(
        "function() local unused = root() return ms.number(426) end",
    );
    assert!(matches!(
        run(&entry).r,
        Err(ImportError::Lua {
            why: LuaWhy::Limit(LuaLimit::Expansion),
            ..
        })
    ));
}

#[test]
fn nested_blocks_count_toward_the_depth_cap() {
    // A helper the first call primes in admission's cache, called again
    // under 120 nested `do` blocks, its own body 120 blocks deep: the
    // statements' nesting alone passes the depth cap.
    let nest = |s: &str| format!("{}{}{}", "do ".repeat(120), s, " end".repeat(120));
    let body = format!("local function h() {} end", nest("return true"));
    let draw = format!("function() h() {} end", nest("return h()"));
    let entry = skin(&body, &image(&draw));
    match run(&entry).r {
        Err(ImportError::Lua {
            why: LuaWhy::Limit(LuaLimit::AstDepth),
            at: Some(at),
        }) => assert_eq!(at.line, destinations(&entry)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_caps_are_fatal_at_the_callable() {
    // 201 nested calls, through global helpers: the depth cap.
    let mut chain = String::from("function h201() return true end\n");
    for k in (1..201).rev() {
        chain.push_str(&format!("function h{k}() return h{}() end\n", k + 1));
    }
    let entry = skin(&chain, &image("function() return h1() end"));
    assert!(matches!(
        run(&entry).r,
        Err(ImportError::Lua {
            why: LuaWhy::Limit(LuaLimit::AstDepth),
            ..
        })
    ));
}
