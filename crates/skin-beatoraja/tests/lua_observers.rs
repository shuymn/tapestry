//! Construction-time observers through the public entries (design.md
//! "beatoraja Lua Skins", observers): a `timer_util.timer_observe_boolean`
//! function at a play destination's `timer` folds when its predicate is
//! constant and writes nothing, false never drawing its destination and
//! true drawing an unconditioned one untimed, from the first loading frame
//! through READY and every practice round; any other is refused at its
//! call. Every fixture is a small self-made skin.

mod common;

use common::play::{Drawn, Loader};
use skin_beatoraja::lua::{Disposition, HOST_APIS, Unsupported};
use skin_beatoraja::{Cancel, Config, ImportError, LuaImported, LuaWhy, read_lua_header};
use skin_model::timer;
use tempfile::TempDir;

/// The lines before a skin's body ([`skin`]): its first line is this
/// plus one.
const HEAD: u32 = 7;

/// A play skin whose configured pass runs `body` with `ms` and `tu` the
/// host modules, source `a` and image `i`.
fn skin(body: &str) -> String {
    format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         local tu = require(\"timer_util\")\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }} }}\n\
         skin.destination = {{}}\n\
         {body}\n\
         return skin\n"
    )
}

/// A skin folder holding `s.luaskin` with `body` ([`skin`]), a 64×64
/// `a.png` and `files`.
fn folder(body: &str, files: &[(&str, &str)]) -> TempDir {
    let d = tempfile::tempdir().unwrap();
    common::write(d.path(), "s.luaskin", skin(body).as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    for (name, text) in files {
        common::write(d.path(), name, text.as_bytes());
    }
    d
}

/// Imports `d`'s `s.luaskin` with its defaults.
fn import(d: &TempDir) -> Result<LuaImported, ImportError> {
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel)?;
    common::lua_play(&h, &Config::defaults(h.header()), &cancel)
}

/// Imports `body` alone.
fn import_body(body: &str) -> Result<LuaImported, ImportError> {
    import(&folder(body, &[]))
}

/// The skin `body` imports, with its folder, which its atlas reads.
fn drawable(body: &str) -> (TempDir, skin_model::Skin) {
    let d = folder(body, &[]);
    let skin = import(&d).unwrap().imported.skin;
    (d, skin)
}

/// Image `i` at `x`, with `fields`, moving from y 1 103 at 0 ms through
/// 600 at 200 ms to rest at 133 from 800 ms (its loop).
fn moving(x: i32, fields: &str) -> String {
    format!(
        "table.insert(skin.destination, {{ id = \"i\", {fields} loop = 800, dst = {{\n\
           {{ time = 0, x = {x}, y = 1103, w = 64, h = 64 }}, {{ time = 200, y = 600 }}, {{ time = 800, y = 133 }} }} }})"
    )
}

/// The y of the rect drawn at `x`, if one is.
fn y_at(rects: &[[i32; 4]], x: i32) -> Option<i32> {
    rects.iter().find(|r| r[0] == x).map(|r| r[1])
}

/// The refusal `r` gives: why and its line in `s.luaskin`.
#[track_caller]
fn refused<T: std::fmt::Debug>(r: Result<T, ImportError>) -> (LuaWhy, u32) {
    match r {
        Err(ImportError::Lua { why, at: Some(at) }) => {
            assert_eq!(at.file, "s.luaskin");
            (why, at.line)
        }
        other => panic!("{other:?}"),
    }
}

/// [`refused`] for an observer the import does not fold.
#[track_caller]
fn observer_refused<T: std::fmt::Debug>(r: Result<T, ImportError>) -> (Unsupported, u32) {
    match refused(r) {
        (
            LuaWhy::Critical {
                field: "timer",
                why,
            },
            line,
        ) => (why, line),
        other => panic!("{other:?}"),
    }
}

/// A practice round's section, 60 s from the chart's start.
fn round() -> eval::Section {
    eval::Section {
        start: bms_model::TimeUs(0),
        end: bms_model::TimeUs(60_000_000),
        lead_in: bms_model::TimeUs(0),
        initial_gauges: vec![common::play::GAUGES[0].start],
    }
}

/// `p`'s rects at READY + 100 ms, its screen origin moved 1 234 ms before
/// READY: an untimed element is at rest there, one restarted at READY is
/// mid-way.
fn at_ready(mut p: Drawn) -> Vec<[i32; 4]> {
    p.screen_from -= 1_234_000;
    p.frame(0, &[]);
    let ready = p.play.timer(timer::READY).expect("ready has started").0 / 1_000;
    p.rects(ready + 100)
}

/// A true observer `O` (x 0) beside its untimed twin `U` (x 100) and a
/// false observer (x 200).
const TWINS: &str = "local on = tu.timer_observe_boolean(function() return true end)\n\
     local off = tu.timer_observe_boolean(function() return nil end)";

#[test]
fn a_true_observer_moves_with_its_untimed_twin_and_a_false_one_never_draws() {
    let body = format!(
        "{TWINS}\n{}\n{}\n{}",
        moving(0, "timer = on,"),
        moving(100, ""),
        moving(200, "timer = off,")
    );
    let d = folder(&body, &[]);
    let i = import(&d).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered, Disposition::Lowered]);
    let skin = i.imported.skin;
    // Loading, 100 ms from its origin: mid-way, the twins together.
    let mut l = Loader::new(skin.clone());
    let rects = l.rects(100);
    let (o, u) = (y_at(&rects, 0), y_at(&rects, 100));
    assert!(o.is_some() && o == u, "{rects:?}");
    assert_ne!(o, y_at(&l.rects(900), 0), "mid-way at 100 ms");
    assert_eq!(y_at(&rects, 200), None);
    // READY + 100 ms, and practice round 2's: at rest, as the twin.
    for rects in [
        at_ready(Drawn::new(skin.clone(), 4)),
        at_ready(Drawn::over_section(skin.clone(), 4, &round())),
    ] {
        let (o, u) = (y_at(&rects, 0), y_at(&rects, 100));
        assert!(o.is_some() && o == u, "{rects:?}");
        assert_eq!(y_at(&rects, 200), None);
    }
}

#[test]
fn a_false_observer_never_draws_whatever_its_conditions() {
    let body = format!("{TWINS}\n{}", moving(200, "op = { 270 }, timer = off,"));
    let (_d, skin) = drawable(&body);
    assert_eq!(y_at(&Loader::new(skin.clone()).rects(100), 200), None);
    assert_eq!(y_at(&at_ready(Drawn::new(skin, 4)), 200), None);
}

#[test]
fn a_minimal_observer_leaves_the_candidate_closed() {
    // A decorative `draw` over a captured local lowers only while the
    // candidate is closed: the observer's root does not open it.
    let body = format!(
        "local x = 1\n{TWINS}\n{}\n{}",
        moving(0, "timer = on,"),
        moving(100, "draw = function() return x == 1 end,")
    );
    let i = import_body(&body).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered, Disposition::Lowered]);
}

#[test]
fn a_predicate_that_writes_is_refused_at_its_call() {
    // Its own upvalue, and through a helper it calls.
    for pre in [
        "local n = 0\nlocal on = tu.timer_observe_boolean(function() n = n + 1 return true end)",
        "local c = 0\nlocal function bump() c = c + 1 end\n\
         local on = tu.timer_observe_boolean(function() bump() return true end)",
    ] {
        let body = format!("{pre}\n{}", moving(0, "timer = on,"));
        let line = HEAD + u32::try_from(pre.lines().count()).unwrap();
        assert_eq!(
            observer_refused(import_body(&body)),
            (Unsupported::Observer("predicate writes"), line),
            "{pre}"
        );
    }
}

#[test]
fn a_later_construction_file_s_assignment_is_the_one_read() {
    // `G` is set by a part run after the observe calls: the final binding
    // folds, as the source reads it each frame.
    let body = format!(
        "local on = tu.timer_observe_boolean(function() return G end)\n\
         local off = tu.timer_observe_boolean(function() return not G end)\n\
         dofile(\"part.lua\")\n{}\n{}",
        moving(0, "timer = on,"),
        moving(200, "timer = off,")
    );
    let d = folder(&body, &[("part.lua", "G = 1\n")]);
    let skin = import(&d).unwrap().imported.skin;
    let rects = Loader::new(skin).rects(100);
    assert!(y_at(&rects, 0).is_some(), "{rects:?}");
    assert_eq!(y_at(&rects, 200), None);
}

#[test]
fn another_root_s_write_of_the_predicate_s_binding_refuses_it() {
    // A decorative `draw` assigning the global opens the candidate; one
    // writing the predicate's upvalue cell voids the proof through it.
    for (pre, writer) in [
        (
            "G = 1\nlocal on = tu.timer_observe_boolean(function() return G end)",
            "draw = function() G = 2 return true end,",
        ),
        (
            "local u = 1\nlocal on = tu.timer_observe_boolean(function() return u end)",
            "draw = function() u = 2 return true end,",
        ),
    ] {
        let body = format!(
            "{pre}\n{}\n{}",
            moving(0, "timer = on,"),
            moving(100, writer)
        );
        let line = HEAD + 2;
        assert_eq!(
            observer_refused(import_body(&body)),
            (Unsupported::Interference, line),
            "{pre}"
        );
    }
}

#[test]
fn a_changing_predicate_is_refused_at_its_call() {
    let body = format!(
        "local on = tu.timer_observe_boolean(function() return ms.number(110) > 0 end)\n{}",
        moving(0, "timer = on,")
    );
    assert_eq!(
        observer_refused(import_body(&body)),
        (Unsupported::Observer("predicate is not constant"), HEAD + 1)
    );
}

#[test]
fn a_true_observer_on_a_conditioned_destination_is_refused() {
    let body = format!("{TWINS}\n{}", moving(0, "op = { 270 }, timer = on,"));
    assert_eq!(
        observer_refused(import_body(&body)),
        (
            Unsupported::Observer("destination is conditioned"),
            HEAD + 1
        )
    );
}

#[test]
fn a_true_observer_whose_object_may_skip_its_timer_is_refused() {
    // A number, at a sentinel value, and an image set whose selector may
    // be negative (ref 40, the gauge kind) leave their preparation before
    // the timer is read, so the source's observer may start late.
    let held = Unsupported::Observer("object may hold its timer back");
    let number = "skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, \
                  digit = 3, value = function() return 1 end } }\n\
                  table.insert(skin.destination, { id = \"v\", timer = on, dst = { { w = 6, h = 6 } } })";
    let set = |r: i32| {
        format!(
            "skin.imageset = {{ {{ id = \"s\", ref = {r}, images = {{ \"i\", \"i\" }} }} }}\n\
             table.insert(skin.destination, {{ id = \"s\", timer = on, dst = {{ {{ w = 8, h = 8 }} }} }})"
        )
    };
    for body in [number.to_owned(), set(40)] {
        let body = format!("{TWINS}\n{body}");
        assert_eq!(
            observer_refused(import_body(&body)),
            (held, HEAD + 1),
            "{body}"
        );
    }
    // A selector never negative (ref 42, the random option) prepares on
    // every frame: the observer folds.
    let body = format!("{TWINS}\n{}", set(42));
    assert_eq!(
        import_body(&body).unwrap().callbacks,
        [Disposition::Lowered]
    );
}

#[test]
fn an_observer_read_anywhere_but_a_timer_is_refused() {
    let body = format!("{TWINS}\n{}", moving(0, "draw = on,"));
    assert_eq!(
        observer_refused(import_body(&body)),
        (
            Unsupported::Observer("timer is no play destination's"),
            HEAD + 1
        )
    );
}

/// Each use of one observer is its own: a timer use elsewhere, at a
/// destination the import keeps or drops, does not admit its use as a
/// `draw` or an `op` entry, which the source reads as a number (truthy).
#[test]
fn every_use_of_an_observer_is_checked() {
    let dropped = "table.insert(skin.destination, { id = \"nope\", timer = on, dst = { { x = 0, y = 0, w = 8, h = 8 } } })";
    let mut imported = Vec::new();
    for (timer, other) in [
        ("timer", moving(0, "timer = on,")),
        ("dropped timer", dropped.to_owned()),
    ] {
        for (use_, field) in [("draw", "draw = on,"), ("op", "op = { on },")] {
            let body = format!("{TWINS}\n{other}\n{}", moving(100, field));
            match import_body(&body) {
                Ok(i) => imported.push(format!("{timer} + {use_}: {:?}", i.callbacks)),
                r => assert_eq!(
                    observer_refused(r),
                    (
                        Unsupported::Observer("timer is no play destination's"),
                        HEAD + 1
                    ),
                    "{body}"
                ),
            }
        }
    }
    assert!(imported.is_empty(), "imported: {imported:#?}");
}

/// A predicate's every reached read is mapped and its operations checked
/// as any lowering's, its value constant or not, a read whose value is
/// discarded included (design.md "beatoraja Lua Skins", the expression
/// acceptance proof): an image index the play has none of (-1) is
/// refused where it is the value and where it is discarded, and so is a
/// read whose availability the constant cannot keep (the maximum BPM,
/// guarded while the range is unknown). A read always available folds.
#[test]
fn a_predicate_s_reads_are_checked_though_its_value_is_constant() {
    let api = u16::try_from(
        HOST_APIS
            .iter()
            .position(|&a| a == ("main_state", "event_index"))
            .unwrap(),
    )
    .unwrap();
    let unmapped = Unsupported::Unmapped { api, id: -1 };
    let mut folded = Vec::new();
    for (predicate, want) in [
        ("return ms.event_index(-1)", unmapped),
        ("local _ = ms.event_index(-1) return true", unmapped),
        (
            "local _ = ms.number(90) return true",
            Unsupported::Observer("predicate reads an availability"),
        ),
    ] {
        let body = format!(
            "local on = tu.timer_observe_boolean(function() {predicate} end)\n{}",
            moving(0, "timer = on,")
        );
        match import_body(&body) {
            Ok(i) => folded.push(format!("{predicate}: {:?}", i.callbacks)),
            r => assert_eq!(observer_refused(r), (want, HEAD + 1), "{predicate}"),
        }
    }
    assert!(folded.is_empty(), "folded: {folded:#?}");
    let body = format!(
        "local on = tu.timer_observe_boolean(function() local _ = ms.number(161) return true end)\n{}",
        moving(0, "timer = on,")
    );
    assert_eq!(
        import_body(&body).unwrap().callbacks,
        [Disposition::Lowered]
    );
}

#[test]
fn an_observer_at_a_destination_the_import_drops_is_no_refusal() {
    // A destination of an image no source defines is skipped, its
    // observer never read.
    let body = format!(
        "{TWINS}
         table.insert(skin.destination, {{ id = \"nope\", timer = on, dst = {{ {{ x = 0, y = 0, w = 8, h = 8 }} }} }})
{}",
        moving(100, "")
    );
    import_body(&body).unwrap();
}

/// An observer held where the source never reads a callback is inert
/// (design.md "beatoraja Lua Skins": a function outside a callback
/// position is inert): stored in the skin table, in a table there, or in
/// a destination field the source does not read, beside its timer use,
/// it refuses nothing.
#[test]
fn an_observer_held_outside_a_callback_position_is_inert() {
    let mut refused = Vec::new();
    for (place, store) in [
        ("a skin field", "skin.saved = on"),
        ("a table", "skin.saved = { on = on }"),
        ("an unread destination field", ""),
    ] {
        let fields = if store.is_empty() {
            "timer = on, saved = on,"
        } else {
            "timer = on,"
        };
        let body = format!("{TWINS}\n{store}\n{}", moving(100, fields));
        if let Err(e) = import_body(&body) {
            refused.push(format!("{place}: {e}"));
        }
    }
    assert!(refused.is_empty(), "refused: {refused:#?}");
}

#[test]
fn the_observe_call_s_arguments_are_checked_at_the_call() {
    let observe = u16::try_from(
        HOST_APIS
            .iter()
            .position(|&a| a == ("timer_util", "timer_observe_boolean"))
            .unwrap(),
    )
    .unwrap();
    for (call, why) in [
        (
            "tu.timer_observe_boolean(1)",
            LuaWhy::Observer("predicate not a function"),
        ),
        (
            "pcall(tu.timer_observe_boolean, 1)",
            LuaWhy::Observer("predicate not a function"),
        ),
        (
            "tu.timer_observe_boolean(string.len)",
            LuaWhy::Observer("predicate not a skin closure"),
        ),
        (
            "tu.timer_observe_boolean(function() return true end)()",
            LuaWhy::LiveState { api: observe },
        ),
    ] {
        assert_eq!(refused(import_body(call)), (why, HEAD + 1), "{call}");
    }
}

#[test]
fn one_returned_function_is_one_observer() {
    let body = format!(
        "{TWINS}\n{}\n{}",
        moving(0, "timer = on,"),
        moving(100, "timer = on,")
    );
    let i = import_body(&body).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered]);
}
