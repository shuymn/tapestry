//! A play slider whose value is a function, against its closure (design.md
//! "beatoraja Lua Skins", the reference executor): the value read only
//! once the slider draws, after its conditions, its timer and its
//! keyframes, as `tofloat()` gives it.

use crate::observed::Cause;
use crate::reference::{TimerRef, Timing, only_value, reaches, shown, timing};
use crate::report::Report;
use crate::script::Script;
use crate::tests::{dst, options, run_with};

/// A slider `id` whose value is `value`, on a destination with `fields`.
fn slider(id: &str, value: &str, fields: &str) -> String {
    format!(
        "table.insert(skin.slider, {{ id = \"{id}\", src = \"a\", x = 0, y = 0, w = 8, h = 8, angle = 2, range = 100,\n\
           value = {value} }})\n{}",
        dst(&format!("\"{id}\""), 0, fields)
    )
}

fn run(sliders: &[String]) -> Report {
    run_with(
        &format!("skin.slider = {{}}\n{}", sliders.concat()),
        &options(Script::full()),
    )
}

/// Asserts `r` passes with `compiled` program callbacks and no listed
/// slider.
fn agrees(r: &Report, compiled: usize) {
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(r.compiled.len(), compiled, "{}", r.text(0, true, true));
    assert!(r.unmodelled.is_empty(), "{}", r.text(0, true, true));
    assert!(r.matched > 0, "{}", r.text(0, true, true));
}

/// A fraction, a floor, and a `nil` branch: each value's `tofloat()`
/// agrees with production's float on every frame.
#[test]
fn slider_values_agree_as_floats() {
    let r = run(&[
        slider("f", "function() return ms.number(100) / 3 + 0.5 end", ""),
        slider(
            "g",
            "function() return math.floor(ms.number(100) / 7) end",
            "",
        ),
        slider(
            "n",
            "function() if ms.number(100) > 100000 then return nil end return ms.number(100) / 1000 end",
            "",
        ),
    ]);
    agrees(&r, 3);
    assert_eq!(r.matched, r.production_runs, "{}", r.text(0, true, true));
}

/// The value runs after the slider's draw and only while it holds
/// (`SkinSlider.java:104-114`): production skips it on the frames the
/// draw is false, and the reference reads it on the others alone.
#[test]
fn a_slider_value_runs_after_its_draw() {
    let value = "function() return ms.number(100) / 3 + 0.5 end";
    let open = run(&[slider("s", value, "")]);
    let gated = run(&[slider(
        "s",
        value,
        "draw = function() return ms.number(100) / 2 >= 50000 end,",
    )]);
    agrees(&open, 1);
    agrees(&gated, 2);
    // The draw runs on every play frame; the value on some.
    assert!(
        gated.production_runs > open.production_runs
            && gated.production_runs < 2 * open.production_runs,
        "{}",
        gated.text(0, true, true)
    );
}

/// The value waits for the destination's timer (`SkinObject.java:352-357`):
/// under the play timer it is not read before play starts.
#[test]
fn a_slider_value_waits_for_its_timer() {
    let value = "function() return ms.number(100) / 3 + 0.5 end";
    let untimed = run(&[slider("s", value, "")]);
    let timed = run(&[slider("s", value, "timer = 41,")]);
    agrees(&untimed, 1);
    agrees(&timed, 1);
    assert!(
        timed.production_runs > 0 && timed.production_runs < untimed.production_runs,
        "{}",
        timed.text(0, true, true)
    );
}

/// The keyframes reach a time as the source's region does
/// (`SkinObject.java:360-375`): before the first key hides, a loop of -1
/// hides past the last, a loop repeats from itself.
#[test]
fn keyframes_reach_the_time() {
    let t = |first, last, looping| Timing {
        timer: TimerRef::None,
        first,
        last,
        looping,
        mouse: false,
    };
    // Held at the end.
    assert!(reaches(&t(0, 1600, 1600), 0));
    assert!(reaches(&t(0, 1600, 1600), 60_000));
    // Before the first key.
    assert!(!reaches(&t(500, 1000, 0), 400));
    assert!(reaches(&t(500, 1000, 0), 600));
    // A loop of -1 ends at the last key.
    assert!(reaches(&t(0, 1000, -1), 1000));
    assert!(!reaches(&t(0, 1000, -1), 1001));
    // A loop from 200 repeats into the gap before the first key at 500:
    // 1100 is 300 again.
    assert!(!reaches(&t(500, 1000, 200), 1100));
    assert!(reaches(&t(500, 1000, 200), 1400));
}

/// A return the slider coerces (design.md "beatoraja Lua Skins", numbers
/// and numeric strings): a numeral string, a hexadecimal one with a sign
/// and whitespace, a string that is no numeral, booleans, a numeric −0
/// (the source's integer zero, +0) and a string's −0 (its sign kept),
/// each as its own slider. Every one agrees on every play frame.
#[test]
fn slider_returns_coerce_as_the_slider_reads_them() {
    let returns = [
        "\"0.25\"",
        "\" -0x1\t\"",
        "\"abc\"",
        "false",
        "true",
        "-0.0",
        "ms.number(100) * -0.0",
        "\"-0.0\"",
    ];
    let sliders: Vec<String> = returns
        .iter()
        .enumerate()
        .map(|(i, v)| slider(&format!("s{i}"), &format!("function() return {v} end"), ""))
        .collect();
    let r = run(&sliders);
    agrees(&r, returns.len());
    assert_eq!(r.matched, r.production_runs, "{}", r.text(0, true, true));
}

/// A boolean `timer` is no timer, as the source's serializer reads it
/// (`LuaSkinLoader.java:155-167`): the value runs on the frames the
/// untimed control runs it, and agrees.
#[test]
fn a_boolean_timer_is_no_timer() {
    let value = "function() return ms.number(100) / 3 + 0.5 end";
    let untimed = run(&[slider("s", value, "")]);
    let boolean = run(&[slider("s", value, "timer = false,")]);
    agrees(&untimed, 1);
    agrees(&boolean, 1);
    assert_eq!(
        boolean.production_runs,
        untimed.production_runs,
        "{}",
        boolean.text(0, true, true)
    );
}

/// A mouse rectangle decides only once the keyframes reach the time
/// (`SkinObject.java:591-607`): hidden before them, undecided after.
#[test]
fn a_mouse_rectangle_decides_after_the_keyframes() {
    let t = Timing {
        timer: TimerRef::None,
        first: 500,
        last: 1000,
        looping: 0,
        mouse: true,
    };
    assert_eq!(shown(&t, 400), Err(Cause::Hidden));
    assert_eq!(shown(&t, 600), Err(Cause::Inconclusive));
    let t = Timing { mouse: false, ..t };
    assert_eq!(shown(&t, 600), Ok(()));
}

/// A keyframe's `Integer.MIN_VALUE` time is absent to the loader
/// (`JSONSkinLoader.java:428-441`): it takes the time before it.
#[test]
fn a_minimum_key_time_is_the_one_before() {
    let lua = mlua::Lua::new();
    let dest: mlua::Table = lua
        .load("return { dst = { { time = 500 }, { time = -2147483648 }, { time = 900 } } }")
        .eval()
        .unwrap();
    let t = timing(&dest);
    assert_eq!((t.first, t.last), (500, 900));
    let dest: mlua::Table = lua
        .load("return { dst = { { time = -2147483648 }, { time = 300 } } }")
        .eval()
        .unwrap();
    let t = timing(&dest);
    assert_eq!((t.first, t.last), (0, 300));
}

/// A slider is modelled only when its own `value` is its one function: a
/// function anywhere else in its definition, nested too, lists it.
#[test]
fn only_a_lone_value_function_is_modelled() {
    let lua = mlua::Lua::new();
    let def = |src: &str| -> mlua::Table { lua.load(src).eval().unwrap() };
    assert!(only_value(&def(
        "return { id = \"s\", value = function() end }"
    )));
    assert!(!only_value(&def(
        "return { id = \"s\", value = function() end, event = function() end }"
    )));
    assert!(!only_value(&def(
        "return { id = \"s\", value = function() end, extra = { function() end } }"
    )));
    assert!(!only_value(&def(
        "return { id = \"s\", value = function() end, extra = { value = function() end } }"
    )));
}

/// A NaN agrees with any NaN: a slider dividing zero by zero, where the
/// reference VM's native division may give another NaN than the
/// program's, agrees on every play frame in its converted value and its
/// output cells.
#[test]
fn a_nan_value_agrees_whatever_its_bits() {
    let r = run(&[slider(
        "s",
        "function() return (ms.number(100) - ms.number(100)) / (ms.number(100) - ms.number(100)) end",
        "",
    )]);
    agrees(&r, 1);
    assert_eq!(r.matched, r.production_runs, "{}", r.text(0, true, true));
}

/// A slider whose definition nests deeper than the scan reads is listed:
/// the scan cannot tell whether a function lies past it.
#[test]
fn a_definition_past_the_scan_is_listed() {
    let lua = mlua::Lua::new();
    let def: mlua::Table = lua
        .load(
            "return { id = \"s\", value = function() end, a = { { { { { { { { 1 } } } } } } } } }",
        )
        .eval()
        .unwrap();
    assert!(!only_value(&def));
    let def: mlua::Table = lua
        .load("return { id = \"s\", value = function() end, a = { { 1 } } }")
        .eval()
        .unwrap();
    assert!(only_value(&def));
}

/// A timer string read as a number is that timer, `"0x29"` timer 41; a
/// negative one is no timer, and any other string a script.
#[test]
fn a_numeric_timer_string_is_an_id() {
    let lua = mlua::Lua::new();
    let timer = |t: &str| {
        timing(
            &lua.load(format!("return {{ timer = {t}, dst = {{ {{}} }} }}"))
                .eval()
                .unwrap(),
        )
        .timer
    };
    assert!(matches!(timer("\"0x29\""), TimerRef::Id(41)));
    assert!(matches!(timer("\" 41 \""), TimerRef::Id(41)));
    assert!(matches!(timer("\"-1\""), TimerRef::None));
    assert!(matches!(timer("\"timer41\""), TimerRef::Undecided));
    assert!(matches!(timer("false"), TimerRef::None));
}
