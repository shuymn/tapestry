//! A play slider whose value is a function, against its closure (design.md
//! "beatoraja Lua Skins", the reference executor): the value read only
//! once the slider draws, after its conditions, its timer and its
//! keyframes, as `tofloat()` gives it.

use crate::reference::{TimerRef, Timing, reaches};
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
