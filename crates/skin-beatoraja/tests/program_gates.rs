//! The presentation program runs a callback only on the play frames the
//! source calls it (design.md "beatoraja Lua Skins", the program): never
//! on an object the source removes at installation, and only past each
//! kind's own prepare gates. Every fixture is a small self-made skin.

mod common;

#[cfg(feature = "dev-tools")]
use bms_model::TimeUs;
use common::play::{Drawn, tally};
#[cfg(feature = "dev-tools")]
use rules::{GaugeValue, Judgment};
use skin_beatoraja::lua::{Disposition, Unsupported};
use skin_beatoraja::{Cancel, Config, ImportError, LuaImported, read_lua_header};
use skin_model::present::Val;
use tempfile::TempDir;

/// A skin folder whose `s.luaskin` is `entry`, with a 64×64 `a.png`.
fn folder(entry: &str) -> TempDir {
    let d = tempfile::tempdir().unwrap();
    common::write(d.path(), "s.luaskin", entry.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    d
}

/// A play skin with an option `fc` (items 905 and 906, 905 the default)
/// running `body` with `ms` the host module, source `a`, images `i` and
/// `g0`..`g6`, and `latch(n)`, a factory of a latch lit once the point
/// reaches `n`.
fn skin(body: &str) -> String {
    format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080,\n\
           property = {{ {{ name = \"fc\", item = {{ {{ name = \"off\", op = 905 }}, {{ name = \"on\", op = 906 }} }} }} }},\n\
           filepath = {{ {{ name = \"mv\", path = \"mv/*\", def = \"clip.mp4\" }} }} }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }} }}\n\
         for k = 0, 6 do table.insert(skin.image, {{ id = \"g\" .. k, src = \"a\", x = 0, y = 0, w = 8, h = 8 }}) end\n\
         local function latch(n)\n\
           local state = ms.timer_off_value\n\
           return function()\n\
             if state == ms.timer_off_value and ms.number(100) >= n then state = ms.time() end\n\
             return state\n\
           end\n\
         end\n\
         local function clock() return ms.time() end\n\
         skin.destination = {{}}\n\
         {body}\n\
         return skin\n"
    )
}

thread_local! {
    /// The folders imported on this thread, kept while the test runs: a
    /// skin reads its files when it is prepared.
    static FOLDERS: std::cell::RefCell<Vec<TempDir>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Imports `body`'s skin with the option `fc` at 905.
fn import(body: &str) -> Result<LuaImported, ImportError> {
    import_files(body, &[])
}

/// [`import`], the skin's folder holding `files` too, each a name and its
/// bytes.
fn import_files(body: &str, files: &[(&str, &[u8])]) -> Result<LuaImported, ImportError> {
    let d = folder(&skin(body));
    for (name, bytes) in files {
        common::write(d.path(), name, bytes);
    }
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel)?;
    FOLDERS.with(|f| f.borrow_mut().push(d));
    let config = Config {
        options: vec![Some(905)],
        ..Config::default()
    };
    common::lua_play(&h, &config, &cancel)
}

/// Whether `r` refused critical callback `field` for `why`.
fn refused(r: &Result<LuaImported, ImportError>, why: &'static str) -> bool {
    matches!(r, Err(ImportError::Lua {
        why: skin_beatoraja::LuaWhy::Critical { why: Unsupported::Construct(w), .. },
        ..
    }) if *w == why)
}

/// `body`'s skin imported, played over a chart of 4 notes.
fn played(body: &str) -> Drawn {
    let i = import(body).unwrap_or_else(|e| panic!("{e:?}"));
    Drawn::new(i.imported.skin, 4)
}

/// A destination of `id` with `fields` and one key at 0.
fn dst(id: &str, fields: &str) -> String {
    format!(
        "table.insert(skin.destination, {{ id = \"{id}\", {fields} dst = {{ {{ x = 0, y = 0, w = 8, h = 8 }} }} }})\n"
    )
}

/// The callbacks the program invokes on frame `ms` (dev-tools).
#[cfg(feature = "dev-tools")]
fn invoked(p: &mut Drawn, ms: i64, effects: &[eval::JudgeEvent]) -> u64 {
    let before = skin_model::present::invocations();
    p.frame(ms, effects);
    skin_model::present::invocations() - before
}

/// `k` of the 4 notes a PERFECT: the point is `k` · 50 000.
fn perfects(p: &mut Drawn, k: u32) {
    p.tally = tally(0, k);
    p.tally.counts.perfect = k;
}

fn construct(why: &'static str) -> Disposition {
    Disposition::Replaced(Unsupported::Construct(why))
}

/// An image set choosing nothing below a point of 100 000.
const NEGATIVE_SET: &str = "skin.imageset = { { id = \"s\", images = { \"i\", \"i\" },\n\
       value = function() if ms.number(100) < 100000 then return -1 end return 0 end } }\n";

// --- The source's discrepancies (§7 of the S7-06e design) -----------------

/// A latch on an image set whose selector can be negative: the source
/// skips it while the selector is negative, so it lights at 2 s, where the
/// program ran it at 1 s. The program cannot gate a slot root, so the
/// latch follows its role: dropped.
#[test]
fn a_latch_under_a_negative_selector_is_refused() {
    let i = import(&format!(
        "{NEGATIVE_SET}table.insert(skin.destination, {{ id = \"s\", timer = latch(1), loop = 2000,\n\
           dst = {{ {{ time = 0, x = 0, y = 0, w = 10, h = 10 }}, {{ time = 2000, x = 100, y = 0, w = 10, h = 10 }} }} }})"
    ))
    .unwrap();
    assert!(
        i.callbacks.contains(&construct(
            "a slot writer the program does not run on every play frame"
        )),
        "{:?}",
        i.callbacks
    );
    assert!(!i.callbacks.contains(&Disposition::Compiled));
}

/// A stateless timer on the same set runs only while it selects.
#[cfg(feature = "dev-tools")]
#[test]
fn a_negative_selector_skips_its_timer() {
    let mut p = played(&format!("{NEGATIVE_SET}{}", dst("s", "timer = clock,")));
    perfects(&mut p, 1);
    p.state_us = 1_000_000;
    assert_eq!(invoked(&mut p, 1_000, &[]), 0);
    perfects(&mut p, 2);
    p.state_us = 2_000_000;
    assert_eq!(invoked(&mut p, 2_000, &[]), 2);
}

/// A judge of grades `g0`..`g6`, grade `k`'s destination with `fields`,
/// the outer destination with `outer`.
fn judge(fields: &[(usize, &str)], outer: &str, slots: usize) -> String {
    let images: String = (0..slots)
        .map(|k| {
            let f = fields.iter().find(|(g, _)| *g == k).map_or("", |(_, f)| *f);
            if f.contains("dst =") {
                format!("{{ id = \"g{k}\", {f} }},")
            } else {
                format!("{{ id = \"g{k}\", {f} dst = {{ {{ x = 0, y = 0, w = 8, h = 8 }} }} }},")
            }
        })
        .collect();
    format!(
        "skin.judge = {{ {{ id = \"j\", index = 0, images = {{ {images} }}, numbers = {{}} }} }}\n{}",
        dst("j", outer)
    )
}

#[cfg(feature = "dev-tools")]
fn at(ms: i64, j: Judgment) -> eval::JudgeEvent {
    use common::play::judged;
    judged(ms, j, 1)
}

/// A draw only the program runs.
#[cfg(feature = "dev-tools")]
const PROGRAM_DRAW: &str = "draw = function() return ms.time() >= 0 end,";

/// Only the selected grade prepares: the GREAT part's draw runs while
/// the latest judgment is a GREAT.
#[cfg(feature = "dev-tools")]
#[test]
fn only_the_selected_grade_runs() {
    let mut p = played(&judge(&[(1, PROGRAM_DRAW)], "", 7));
    assert_eq!(invoked(&mut p, 1_000, &[at(1_000, Judgment::Perfect)]), 0);
    assert_eq!(invoked(&mut p, 2_000, &[at(2_000, Judgment::Great)]), 1);
    assert_eq!(invoked(&mut p, 2_100, &[]), 1);
}

/// Before any judgment the judge prepares nothing, its outer conditions
/// included.
#[cfg(feature = "dev-tools")]
#[test]
fn no_judgment_runs_no_outer_callback() {
    let mut p = played(&judge(
        &[],
        "draw = function() return ms.time() >= 0 end,",
        1,
    ));
    assert_eq!(invoked(&mut p, 500, &[]), 0);
    assert_eq!(invoked(&mut p, 1_000, &[at(1_000, Judgment::Great)]), 1);
}

/// A number whose object the source removes at installation never reads
/// its value: not selected, undeclared, without keys; the one installed
/// does.
#[cfg(feature = "dev-tools")]
#[test]
fn installation_removes_a_number_s_value() {
    let number = "skin.value = { { id = \"m\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4,\n\
         value = function() return ms.number(100) / 2 end } }\n";
    let body = format!(
        "{number}{}{}table.insert(skin.destination, {{ id = \"m\", dst = {{}} }})\n{}",
        dst("m", "op = { 906 },"),
        dst("m", "op = { -950 },"),
        dst("m", "op = { 905 },"),
    );
    let mut p = played(&body);
    assert_eq!(invoked(&mut p, 1_000, &[]), 1);
}

// --- R0: installation ------------------------------------------------------

/// An image set every entry of which is a hole is removed before its
/// unsupported blend is read; its latch is no site, so the same latch on a
/// plain image after it is the only one, and runs once a frame.
#[cfg(feature = "dev-tools")]
#[test]
fn a_removed_site_does_not_hide_a_drop() {
    let body = format!(
        "skin.imageset = {{ {{ id = \"h\", images = {{ \"nope\", \"none\" }} }} }}\n\
         local l = latch(100000)\n{}{}",
        dst("h", "timer = l, blend = 9,"),
        dst("i", "timer = l,")
    );
    let i = import(&body).unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled]);
    let mut p = Drawn::new(i.imported.skin, 4);
    // Off: invoked once, not twice.
    assert_eq!(invoked(&mut p, 1_000, &[]), 1);
}

/// Latch A on a removed destination compiles nothing, so B takes the
/// first slot and A, on a kept destination after it, its own.
#[test]
fn interleaved_latches_keep_their_own_slots() {
    let factory = |n: u32| {
        format!(
            "(function() local s = ms.timer_off_value return function()\n\
               if s == ms.timer_off_value and ms.number(100) >= {n} then s = ms.time() end return s end end)()"
        )
    };
    let body = format!(
        "local a = {}\nlocal b = {}\n{}{}{}",
        factory(200_000),
        factory(100_000),
        dst("i", "timer = a, op = { 906 },"),
        dst("i", "timer = b,"),
        dst("i", "timer = a,")
    );
    let mut p = played(&body);
    let bank = |p: &Drawn| p.play.bank().values().to_vec();
    p.state_us = 1_000_000;
    perfects(&mut p, 2);
    p.frame(1_000, &[]);
    assert_eq!(bank(&p), [Val::Int(1_000_000), skin_model::present::OFF]);
    p.state_us = 2_000_000;
    perfects(&mut p, 4);
    p.frame(2_000, &[]);
    assert_eq!(bank(&p), [Val::Int(1_000_000), Val::Int(2_000_000)]);
}

/// A judge with no authored key keeps its constructor's key and its
/// grades, but its own `op`, `draw` and `timer` are inert: an option it
/// does not select does not remove it, and its draw never runs.
#[cfg(feature = "dev-tools")]
#[test]
fn a_judge_without_keys_is_installed_its_callbacks_inert() {
    let body = "skin.judge = { { id = \"j\", index = 0, images = { { id = \"g0\", dst = { { x = 0, y = 0, w = 8, h = 8 } } } }, numbers = {} } }\n\
         table.insert(skin.destination, { id = \"j\", op = { 906 }, draw = function() return ms.time() >= 0 end, dst = {} })";
    let mut p = played(body);
    assert_eq!(invoked(&mut p, 1_000, &[at(1_000, Judgment::Perfect)]), 0);
    assert!(!p.rects(1_001).is_empty());
}

/// A chart fact the source prunes by gates the value for the attempt.
#[cfg(feature = "dev-tools")]
#[test]
fn a_chart_static_condition_gates_the_value() {
    let body = "skin.value = { { id = \"m\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4,\n\
         value = function() return ms.number(100) / 2 end } }\n"
        .to_owned()
        + &dst("m", "op = { 151 },");
    let skin = import(&body).unwrap().imported.skin;
    let chart = common::play::chart(4);
    for (difficulty, want) in [(Some(1), 1), (Some(2), 0)] {
        let meta = skin_model::ChartMetadata {
            difficulty,
            ..skin_model::ChartMetadata::UNKNOWN
        };
        let mut p = Drawn::over_meta(skin.clone(), &chart, meta);
        assert_eq!(invoked(&mut p, 1_000, &[]), want, "{difficulty:?}");
        assert_eq!(invoked(&mut p, 1_016, &[]), want, "{difficulty:?}");
    }
}

/// A false static condition removes the object, a name or negation
/// keeping its property's class.
#[cfg(feature = "dev-tools")]
#[test]
fn static_folds_remove_the_object() {
    let number = "skin.value = { { id = \"m\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4,\n\
         value = function() return ms.number(100) / 2 end } }\n";
    for (fields, want) in [
        ("op = { 1008 },", 0),
        ("op = { \"table_song\" },", 0),
        ("op = { -1008 },", 1),
        ("op = { \"!table_song\" },", 1),
        ("draw = 290,", 0),
        // The course stages are static too (S10-13b), and false.
        ("op = { 281 },", 0),
        ("op = { \"course_stage_final\" },", 0),
        ("op = { -289 },", 1),
    ] {
        let mut p = played(&format!("{number}{}", dst("m", fields)));
        assert_eq!(invoked(&mut p, 1_000, &[]), want, "{fields}");
    }
}

/// A system image with a program condition and a false static one: the
/// source removes it, so the condition is never compiled.
#[cfg(feature = "dev-tools")]
#[test]
fn a_system_image_is_removed_too() {
    let mut p = played(&dst(
        "-110",
        "op = { function() return ms.time() >= 0 end, \"table_song\" },",
    ));
    assert_eq!(invoked(&mut p, 1_000, &[]), 0);
    let mut p = played(&dst(
        "-110",
        "op = { function() return ms.time() >= 0 end },",
    ));
    assert_eq!(invoked(&mut p, 1_000, &[]), 1);
}

/// A missing image sharing a valid judge's id keeps the existing refusal
/// rather than being removed.
#[test]
fn a_missing_image_sharing_a_judge_s_id_still_refuses() {
    let body = format!(
        "table.insert(skin.image, {{ id = \"j\", src = \"nope\", x = 0, y = 0, w = 8, h = 8 }})\n{}",
        judge(&[], "", 1)
    );
    assert!(matches!(
        import(&body),
        Err(ImportError::Critical {
            part: skin_beatoraja::CriticalPart::Judge,
            ..
        })
    ));
}

/// A use on a kept destination keeps its outcome when the same callable
/// is also on a removed one, read before or after it.
#[test]
fn a_kept_use_keeps_its_outcome() {
    for body in [
        dst("i", "timer = clock,") + &dst("i", "timer = clock, op = { 906 },"),
        dst("i", "timer = clock, op = { 906 },") + &dst("i", "timer = clock,"),
    ] {
        let i = import(&body).unwrap();
        assert_eq!(i.callbacks, [Disposition::Compiled], "{body}");
    }
}

/// A slot root's consumer on a destination whose element the import does
/// not retain (a missing file it could not prove before reading it) counts
/// as a dropped destination's.
#[test]
fn an_uncertain_dropped_site_refuses_its_latch() {
    let body = format!(
        "table.insert(skin.source, {{ id = \"m\", path = \"missing.png\" }})\n\
         table.insert(skin.image, {{ id = \"x\", src = \"m\", x = 0, y = 0, w = 8, h = 8 }})\n\
         local l = latch(100000)\n{}{}",
        dst("x", "timer = l,"),
        dst("i", "timer = l,")
    );
    let i = import(&body).unwrap();
    assert!(
        i.callbacks
            .contains(&construct("a slot writer a dropped destination calls")),
        "{:?}",
        i.callbacks
    );
}

// --- Per-kind gates ----------------------------------------------------------

/// A plain image's `ref` is inert: its latch needs no gate.
#[test]
fn a_plain_image_ignores_its_ref() {
    let i = import(
        &("table.insert(skin.image, { id = \"r\", src = \"a\", x = 0, y = 0, w = 8, h = 8, ref = 40 })\n".to_owned()
            + &dst("r", "timer = latch(100000),")),
    )
    .unwrap();
    assert_eq!(i.callbacks, [Disposition::Compiled]);
}

/// A grouped image selecting by the gauge's type can select nothing, so
/// its latch has a gate and follows its role; one selecting by the cover
/// switch never does.
#[test]
fn a_grouped_image_gates_on_its_selector() {
    for (r, want) in [(40, false), (330, true)] {
        let i = import(&format!(
            "table.insert(skin.image, {{ id = \"r\", src = \"a\", x = 0, y = 0, w = 64, h = 8, divx = 8, len = 2, ref = {r} }})\n{}",
            dst("r", "timer = latch(100000),")
        ))
        .unwrap();
        assert_eq!(
            i.callbacks == [Disposition::Compiled],
            want,
            "{r}: {:?}",
            i.callbacks
        );
    }
}

/// An image set's `value` property overrides its `ref`: a value keeps the
/// gate, a non-negative `ref` alone does not.
#[test]
fn an_image_set_gates_on_value_then_ref() {
    for (fields, want) in [
        ("ref = 330", true),
        ("value = 525, ref = 330", false),
        ("value = \"judge_timing\", ref = 330", false),
    ] {
        let i = import(&format!(
            "skin.imageset = {{ {{ id = \"s\", images = {{ \"i\" }}, {fields} }} }}\n{}",
            dst("s", "timer = latch(100000),")
        ))
        .unwrap();
        assert_eq!(
            i.callbacks == [Disposition::Compiled],
            want,
            "{fields}: {:?}",
            i.callbacks
        );
    }
}

/// A selector naming a hole entry still runs the conditions and the
/// timer: the hole hides the object only after them.
#[cfg(feature = "dev-tools")]
#[test]
fn selecting_a_hole_still_runs_its_callbacks() {
    let mut p = played(&format!(
        "skin.imageset = {{ {{ id = \"s\", images = {{ \"nope\", \"i\" }}, value = function() return 0 end }} }}\n{}",
        dst("s", "timer = clock,")
    ));
    assert_eq!(invoked(&mut p, 1_000, &[]), 2);
    assert!(p.rects(1_001).is_empty());
}

/// A program callback after a read the source sees as a pre-judgment
/// sentinel follows its role; a judge part's, gated after a judgment,
/// does not.
#[test]
fn sentinel_reads_drop_later_program_callbacks() {
    let draw = "draw = function() return ms.time() >= 0 end,";
    let sentinel = construct("a program callback behind a pre-judgment sentinel");
    for body in [
        format!(
            "skin.value = {{ {{ id = \"m\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4, ref = 102 }} }}\n{}",
            dst("m", draw)
        ),
        format!(
            "skin.value = {{ {{ id = \"m\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4,\n\
               value = function() return ms.number(115) / 1 end }} }}\n{}",
            dst("m", draw)
        ),
        format!(
            "skin.imageset = {{ {{ id = \"s\", images = {{ \"i\" }}, value = 155 }} }}\n{}",
            dst("s", "timer = clock,")
        ),
        format!(
            "local function rate() return ms.number(102) end\n\
             skin.imageset = {{ {{ id = \"s\", images = {{ \"i\" }}, value = function() if rate() > 50 then return 0 end return 0 end }} }}\n{}",
            dst("s", "timer = clock,")
        ),
    ] {
        let i = import(&body).unwrap();
        assert!(i.callbacks.contains(&sentinel), "{body}: {:?}", i.callbacks);
    }
    // The EX score's number is critical: it refuses.
    assert!(refused(
        &import(&format!(
            "skin.value = {{ {{ id = \"m\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4, ref = 101 }} }}\n{}",
            dst("m", draw)
        )),
        "a program callback behind a pre-judgment sentinel"
    ));
    // The same with a lowered draw imports.
    let i = import(&format!(
        "skin.value = {{ {{ id = \"m\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4, ref = 102 }} }}\n{}",
        dst("m", "draw = function() return ms.number(100) > 0 end,")
    ))
    .unwrap();
    assert!(!i.callbacks.contains(&sentinel));
    // A grade's draw behind a read of the EX score.
    let i = import(&judge(
        &[(0, "op = { function() return ms.number(71) > 0 end }, draw = function() return ms.time() >= 0 end,")],
        "",
        1,
    ))
    .unwrap();
    assert!(
        i.callbacks.contains(&Disposition::Compiled),
        "{:?}",
        i.callbacks
    );
}

/// The outer conditions do not gate the selected grade.
#[cfg(feature = "dev-tools")]
#[test]
fn a_false_outer_visibility_keeps_the_grade() {
    let mut p = played(&judge(
        &[(0, PROGRAM_DRAW)],
        "draw = function() return ms.number(100) < 0 end,",
        1,
    ));
    assert_eq!(invoked(&mut p, 1_000, &[at(1_000, Judgment::Perfect)]), 1);
}

/// A full gauge's PERFECT is slot 6's when the source has a part there,
/// even one the import drops (its track never drawn).
#[cfg(feature = "dev-tools")]
#[test]
fn perfect_at_max_selects_slot_six_or_perfect() {
    // Every key after its loop point: never drawn.
    let never = "loop = 50, dst = { { time = 100, x = 0, y = 0, w = 8, h = 8 } }";
    for (slots, six, want) in [(1, "", 1), (7, "", 0), (7, never, 0)] {
        let mut fields = vec![(0, PROGRAM_DRAW)];
        if !six.is_empty() {
            fields.push((6, six));
        }
        let mut p = played(&judge(&fields, "", slots));
        p.gauges[0].value = GaugeValue::FULL;
        let max = eval::JudgeEvent {
            seq: 0,
            time: TimeUs(900_000),
            origin: None,
            kind: eval::JudgeEventKind::Gauge {
                gauge: 0,
                value: GaugeValue::FULL,
                reference: GaugeValue::FULL,
            },
        };
        p.frame(900, &[max]);
        assert_eq!(
            invoked(&mut p, 1_000, &[at(1_000, Judgment::Perfect)]),
            want,
            "{slots} {six}"
        );
    }
}

/// A combo's program callback cannot be gated on its grade drawing: the
/// judge refuses.
#[test]
fn a_judge_combo_program_callback_refuses() {
    let body = "skin.value = { { id = \"c\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2 } }\n\
         skin.judge = { { id = \"j\", index = 0, images = { { id = \"g0\", dst = { { x = 0, y = 0, w = 8, h = 8 } } } },\n\
           numbers = { { id = \"c\", draw = function() return ms.time() >= 0 end, dst = { { x = 0, y = 0, w = 4, h = 8 } } } } } }\n"
        .to_owned()
        + &dst("j", "");
    assert!(refused(
        &import(&body),
        "a judge combo's callback the program cannot gate"
    ));
}

/// A value slider: its value read only while it draws, for the shapes the
/// program can gate; any other follows its role.
fn slider(fields: &str, keys: &str) -> String {
    format!(
        "skin.slider = {{ {{ id = \"v\", src = \"a\", x = 0, y = 0, w = 8, h = 8, angle = 1, range = 100,\n\
           value = function() return ms.time() / 1000000 end }} }}\n\
         table.insert(skin.destination, {{ id = \"v\", {fields} dst = {{ {keys} }} }})\n"
    )
}

const KEY0: &str = "{ x = 0, y = 0, w = 8, h = 8 }";

#[cfg(feature = "dev-tools")]
#[test]
fn a_value_slider_reads_its_value_only_when_drawn() {
    // Untimed: every frame its conditions hold.
    let mut p = played(&slider("", KEY0));
    assert_eq!(invoked(&mut p, 1_000, &[]), 1);
    let mut p = played(&slider(
        "draw = function() return ms.number(100) > 0 end,",
        KEY0,
    ));
    assert_eq!(invoked(&mut p, 1_000, &[]), 0);
    perfects(&mut p, 1);
    assert_eq!(invoked(&mut p, 1_016, &[]), 1);
    // A program draw before it is its `after`.
    let mut p = played(&slider(
        "draw = function() return ms.time() >= 2000000 end,",
        KEY0,
    ));
    p.state_us = 1_000_000;
    assert_eq!(invoked(&mut p, 1_000, &[]), 1);
    p.state_us = 2_000_000;
    assert_eq!(invoked(&mut p, 1_016, &[]), 2);
}

/// On the last note's timer (143): not before the notes' end, and not
/// after a rearm clears it.
#[cfg(feature = "dev-tools")]
#[test]
fn a_value_slider_on_the_last_note() {
    let skin = import(&slider("timer = 143,", KEY0)).unwrap().imported.skin;
    let chart = common::play::chart(4);
    let mut p = Drawn::over_ending(skin, &chart, TimeUs(2_000_000));
    assert_eq!(invoked(&mut p, 1_999, &[]), 0);
    assert_eq!(invoked(&mut p, 2_000, &[]), 1);
    p.play.rearm();
    assert_eq!(invoked(&mut p, 1_000, &[]), 0);
}

#[test]
fn value_slider_shapes_without_a_gate_are_dropped() {
    let keys = construct("a slider value whose keys need the source's prepare clock");
    let timer = construct("a slider value on a timer the program cannot gate");
    for (fields, keys_, want) in [
        (
            "loop = -1,",
            "{ x = 0, y = 0, w = 8, h = 8 }, { time = 1000, x = 1, y = 0, w = 8, h = 8 }",
            &keys,
        ),
        (
            "timer = 46, loop = -1,",
            "{ x = 0, y = 0, w = 8, h = 8 }, { time = 1, x = 0, y = 0, w = 8, h = 8 }",
            &keys,
        ),
        (
            "timer = 41, loop = -1,",
            "{ x = 0, y = 0, w = 8, h = 8 }, { time = 14400000, x = 0, y = 0, w = 8, h = 8 }",
            &keys,
        ),
        (
            "loop = 200,",
            "{ time = 100, x = 0, y = 0, w = 8, h = 8 }, { time = 200, x = 0, y = 0, w = 8, h = 8 }",
            &keys,
        ),
        ("timer = 46,", KEY0, &timer),
        ("timer = 101,", KEY0, &timer),
        ("timer = clock,", KEY0, &timer),
    ] {
        let i = import(&slider(fields, keys_)).unwrap();
        assert!(i.callbacks.contains(want), "{fields}: {:?}", i.callbacks);
    }
}

/// A dropped kind compiles no callback.
#[test]
fn a_dropped_kind_compiles_no_callback() {
    let i = import(
        &("skin.graph = { { id = \"gr\", src = \"a\", x = 0, y = 0, w = 8, h = 8, type = 0 } }\n"
            .to_owned()
            + &dst("gr", "draw = function() return ms.time() >= 0 end,")),
    )
    .unwrap();
    assert!(
        !i.callbacks.contains(&Disposition::Compiled),
        "{:?}",
        i.callbacks
    );
}

/// Slot 6's part exists when its texture loads, whatever the import makes
/// of its track: a loaded part never drawn still takes a full gauge's
/// PERFECT; an absent texture leaves it to slot 0.
#[cfg(feature = "dev-tools")]
#[test]
fn slot_six_exists_by_its_texture() {
    let never = "loop = 50, dst = { { time = 100, x = 0, y = 0, w = 8, h = 8 } }";
    for (src, want) in [("a", 0), ("m", 1)] {
        let images: String = (0..6)
            .map(|k| {
                let f = if k == 0 { PROGRAM_DRAW } else { "" };
                format!("{{ id = \"g{k}\", {f} dst = {{ {{ x = 0, y = 0, w = 8, h = 8 }} }} }},")
            })
            .collect();
        let body = format!(
            "table.insert(skin.source, {{ id = \"m\", path = \"missing.png\" }})\n\
             table.insert(skin.image, {{ id = \"six\", src = \"{src}\", x = 0, y = 0, w = 8, h = 8 }})\n\
             skin.judge = {{ {{ id = \"j\", index = 0, images = {{ {images} {{ id = \"six\", {never} }} }}, numbers = {{}} }} }}\n{}",
            dst("j", "")
        );
        let mut p = played(&body);
        full_gauge(&mut p);
        assert_eq!(
            invoked(&mut p, 1_000, &[at(1_000, Judgment::Perfect)]),
            want,
            "{src}"
        );
    }
}

/// The gauge full from 900 ms.
#[cfg(feature = "dev-tools")]
fn full_gauge(p: &mut Drawn) {
    p.gauges[0].value = GaugeValue::FULL;
    let max = eval::JudgeEvent {
        seq: 0,
        time: TimeUs(900_000),
        origin: None,
        kind: eval::JudgeEventKind::Gauge {
            gauge: 0,
            value: GaugeValue::FULL,
            reference: GaugeValue::FULL,
        },
    };
    p.frame(900, &[max]);
}

/// A movie ignores `len`, so an empty run proves nothing: the image is
/// uncertain, its timer a site, and a latch it shares with a kept image
/// is refused once the import drops the movie.
#[test]
fn a_movie_grouped_image_is_not_removed() {
    let body = format!(
        "table.insert(skin.source, {{ id = \"v\", path = \"a.mp4\" }})\n\
         table.insert(skin.image, {{ id = \"mv\", src = \"v\", x = 0, y = 0, w = 8, h = 8, len = 2 }})\n\
         local l = latch(100000)\n{}{}",
        dst("mv", "timer = l,"),
        dst("i", "timer = l,")
    );
    let i = import(&body).unwrap();
    assert!(
        i.callbacks
            .contains(&construct("a slot writer a dropped destination calls")),
        "{:?}",
        i.callbacks
    );
}

/// A critical slider whose keys start at 1 s is settled to its last key
/// (D20), but the source reads its value only from 1 s: it refuses.
#[test]
fn a_settled_late_start_slider_refuses() {
    let body = "skin.slider = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 8, h = 8, angle = 1, range = 100,\n\
           value = function() return ms.number(75) / 100 end } }\n\
         table.insert(skin.destination, { id = \"v\", loop = 2000,\n\
           dst = { { time = 1000, x = 0, y = 0, w = 8, h = 8 }, { time = 2000, x = 100, y = 0, w = 8, h = 8 } } })\n";
    assert!(refused(
        &import(body),
        "a slider value whose keys need the source's prepare clock"
    ));
}

/// The accepted deviations are noted at their callbacks: a draw on an
/// image whose declared file does not load (the source makes no object),
/// and a set's timer behind a selector that waits for an availability.
#[test]
fn schedule_deviations_are_noted_at_their_callbacks() {
    use skin_beatoraja::{NoteKind, ScheduleWhy, Severity};
    let noted = |body: &str, field: &'static str, why: ScheduleWhy| {
        let i = import(body).unwrap();
        assert!(
            i.imported
                .report
                .notes
                .iter()
                .any(|n| n.severity == Severity::Deviation
                    && n.at.is_some()
                    && n.kind == NoteKind::Schedule { field, why }),
            "{body}: {:#?}",
            i.imported.report.notes
        );
    };
    noted(
        &format!(
            "table.insert(skin.source, {{ id = \"m\", path = \"missing.png\" }})\n\
             table.insert(skin.image, {{ id = \"x\", src = \"m\", x = 0, y = 0, w = 8, h = 8 }})\n{}",
            dst("x", "draw = function() return ms.time() >= 0 end,")
        ),
        "draw",
        ScheduleWhy::MayBeRemoved,
    );
    noted(
        &format!(
            "skin.imageset = {{ {{ id = \"s\", images = {{ \"i\" }}, value = function() if ms.number(90) > 100 then return 0 end return 0 end }} }}\n{}",
            dst("s", "timer = clock,")
        ),
        "timer",
        ScheduleWhy::Availability,
    );
    // A kept element with a plain selector has no such note.
    let i = import(&dst("i", "draw = function() return ms.time() >= 0 end,")).unwrap();
    assert!(
        !i.imported
            .report
            .notes
            .iter()
            .any(|n| matches!(n.kind, NoteKind::Schedule { .. }))
    );
}

/// On READY (40) and PLAY (41): not before the attempt's start, and not
/// on a rearmed frame before it again.
#[cfg(feature = "dev-tools")]
#[test]
fn a_value_slider_on_ready_and_play() {
    for timer in ["timer = 40,", "timer = 41,"] {
        let mut p = played(&slider(timer, KEY0));
        assert_eq!(invoked(&mut p, -1, &[]), 0, "{timer}");
        assert_eq!(invoked(&mut p, 0, &[]), 1, "{timer}");
        p.play.rearm();
        assert_eq!(invoked(&mut p, -1, &[]), 0, "{timer}");
    }
}

/// A custom timer resolving to PLAY is no proven timer: the value follows
/// its role.
#[test]
fn a_value_slider_on_a_custom_alias_is_dropped() {
    let body = "skin.customTimers = { { id = 10000, timer = 41 } }\n".to_owned()
        + &slider("timer = 10000,", KEY0);
    let i = import(&body).unwrap();
    assert!(
        i.callbacks.contains(&construct(
            "a slider value on a timer the program cannot gate"
        )),
        "{:?}",
        i.callbacks
    );
}

/// A 64x64 PNG whose header is whole and whose pixel data is corrupt.
#[cfg(feature = "dev-tools")]
fn corrupt_png() -> Vec<u8> {
    let d = tempfile::tempdir().unwrap();
    common::png(d.path(), "p.png", 64, 64);
    let mut bytes = std::fs::read(d.path().join("p.png")).unwrap();
    let idat = bytes.windows(4).position(|w| w == b"IDAT").unwrap();
    for b in bytes.iter_mut().skip(idat + 4).take(16) {
        *b ^= 0x5a;
    }
    bytes
}

/// A judge whose slot 6 is image `six`, of source `src`, never drawn, and
/// whose slot 0 has a program draw.
#[cfg(feature = "dev-tools")]
fn six_judge(src: &str) -> String {
    let images: String = (0..6)
        .map(|k| {
            let f = if k == 0 {
                "draw = function() return ms.time() >= 0 end,"
            } else {
                ""
            };
            format!("{{ id = \"g{k}\", {f} dst = {{ {{ x = 0, y = 0, w = 8, h = 8 }} }} }},")
        })
        .collect();
    format!(
        "table.insert(skin.source, {{ id = \"x\", path = \"{src}\" }})\n\
         table.insert(skin.image, {{ id = \"six\", src = \"x\", x = 0, y = 0, w = 8, h = 8 }})\n\
         skin.judge = {{ {{ id = \"j\", index = 0, images = {{ {images} {{ id = \"six\", loop = 50, dst = {{ {{ time = 100, x = 0, y = 0, w = 8, h = 8 }} }} }} }}, numbers = {{}} }} }}\n{}",
        dst("j", "")
    )
}

/// Slot 6's texture decides PERFECT's part at a full gauge whatever the
/// import makes of its track: one that decodes takes it from slot 0; a
/// missing file, a movie (there or not), an unmatched pattern, a file
/// whose pixels do not decode, a file choice naming a movie leave it to
/// slot 0; a pattern some file matches is a random pick the import cannot
/// tell, and PERFECT's program callbacks follow their role.
#[cfg(feature = "dev-tools")]
#[test]
fn slot_six_follows_its_texture() {
    let corrupt = corrupt_png();
    let mp4: &[u8] = b"not a video";
    for (src, files, want) in [
        ("a.png", vec![], 0),
        ("missing.png", vec![], 1),
        ("gone.mp4", vec![], 1),
        ("pics/*.png", vec![], 1),
        ("bad.png", vec![("bad.png", corrupt.as_slice())], 1),
    ] {
        let i = import_files(&six_judge(src), &files).unwrap_or_else(|e| panic!("{src}: {e:?}"));
        let mut p = Drawn::new(i.imported.skin, 4);
        full_gauge(&mut p);
        assert_eq!(
            invoked(&mut p, 1_000, &[at(1_000, Judgment::Perfect)]),
            want,
            "{src}"
        );
    }
    // Not proven either way: a pattern some file matches, and a movie by
    // its name that is there, whatever its bytes (the texture getter
    // decodes it unless a movie object of it came first).
    let png = std::fs::read(FOLDERS.with(|f| f.borrow()[0].path().join("a.png"))).unwrap();
    for (src, files) in [
        ("pics/*.png", vec![("pics/one.png", png.as_slice())]),
        ("clip.mp4", vec![("clip.mp4", mp4)]),
        ("clip.mp4", vec![("clip.mp4", png.as_slice())]),
        ("mv/*", vec![("mv/clip.mp4", mp4)]),
    ] {
        assert!(
            refused(
                &import_files(&six_judge(src), &files),
                "a judge whose slot-6 texture the import cannot prove"
            ),
            "{src}"
        );
    }
}

/// A grouped image whose file choice names a movie is a movie: `len` does
/// not remove it, its timer stays a site, and the latch it shares with a
/// kept image is refused once the import drops the movie.
#[test]
fn a_movie_chosen_by_a_file_choice_is_not_removed() {
    let body = format!(
        "table.insert(skin.source, {{ id = \"v\", path = \"mv/*\" }})\n\
         table.insert(skin.image, {{ id = \"mv\", src = \"v\", x = 0, y = 0, w = 8, h = 8, len = 2 }})\n\
         local l = latch(100000)\n{}{}",
        dst("mv", "timer = l,"),
        dst("i", "timer = l,")
    );
    let i = import_files(&body, &[("mv/clip.mp4", b"not a video")]).unwrap();
    assert!(
        i.callbacks
            .contains(&construct("a slot writer a dropped destination calls")),
        "{:?}",
        i.callbacks
    );
}

/// A set choosing by a guarded property, given as an id, a name or a
/// callable, notes its program callbacks as skipped while unavailable.
#[test]
fn a_guarded_selector_is_noted_whatever_backs_it() {
    use skin_beatoraja::{NoteKind, ScheduleWhy};
    for value in [
        "90",
        "\"maxbpm\"",
        "function() if ms.number(90) > 100 then return 0 end return 0 end",
    ] {
        let i = import(&format!(
            "skin.imageset = {{ {{ id = \"s\", images = {{ \"i\" }}, value = {value} }} }}\n{}",
            dst("s", "timer = clock,")
        ))
        .unwrap();
        assert!(
            i.imported.report.notes.iter().any(|n| n.kind
                == NoteKind::Schedule {
                    field: "timer",
                    why: ScheduleWhy::Availability
                }),
            "{value}"
        );
    }
}

/// A judge is installed whatever its grades: one whose grades the import
/// never draws has no element, but its outer callbacks run where the
/// source runs them, unnoted.
#[test]
fn a_judge_without_drawn_grades_is_not_noted() {
    use skin_beatoraja::NoteKind;
    let i = import(&format!(
        "skin.judge = {{ {{ id = \"j\", index = 0, images = {{ {{ id = \"g0\", loop = 50, dst = {{ {{ time = 100, x = 0, y = 0, w = 8, h = 8 }} }} }} }}, numbers = {{}} }} }}\n{}",
        dst("j", "draw = function() return ms.time() >= 0 end,")
    ))
    .unwrap();
    assert!(
        !i.imported
            .report
            .notes
            .iter()
            .any(|n| matches!(n.kind, NoteKind::Schedule { .. })),
        "{:#?}",
        i.imported.report.notes
    );
}

/// A refused slot root's consumers never run: no schedule note for them.
#[test]
fn a_refused_root_is_not_noted() {
    use skin_beatoraja::NoteKind;
    let body = format!(
        "table.insert(skin.source, {{ id = \"m\", path = \"missing.png\" }})\n\
         table.insert(skin.image, {{ id = \"x\", src = \"m\", x = 0, y = 0, w = 8, h = 8 }})\n\
         local l = latch(100000)\n{}{}",
        dst("x", "timer = l,"),
        dst("i", "timer = l,")
    );
    let i = import(&body).unwrap();
    assert!(
        !i.imported
            .report
            .notes
            .iter()
            .any(|n| matches!(n.kind, NoteKind::Schedule { .. })),
        "{:#?}",
        i.imported.report.notes
    );
}

/// A pattern whose folder holds only a link out of the skin folder: the
/// listing skips it where the source would pick it, so the import cannot
/// prove the image absent. Its timer stays a site, and the latch it shares
/// with a kept image is refused once the import drops it.
#[test]
fn a_pattern_with_a_skipped_entry_is_not_removed() {
    let outside = tempfile::tempdir().unwrap();
    common::png(outside.path(), "far.png", 8, 8);
    let body = format!(
        "table.insert(skin.source, {{ id = \"g\", path = \"pics/*.png\" }})\n\
         table.insert(skin.image, {{ id = \"x\", src = \"g\", x = 0, y = 0, w = 8, h = 8 }})\n\
         local l = latch(100000)\n{}{}",
        dst("x", "timer = l,"),
        dst("i", "timer = l,")
    );
    let d = folder(&skin(&body));
    std::fs::create_dir_all(d.path().join("pics")).unwrap();
    let target = outside.path().join("far.png");
    let link = d.path().join("pics").join("near.png");
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target, &link).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let config = Config {
        options: vec![Some(905)],
        ..Config::default()
    };
    let i = common::lua_play(&h, &config, &cancel).unwrap();
    assert!(
        i.callbacks
            .contains(&construct("a slot writer a dropped destination calls")),
        "{:?}",
        i.callbacks
    );
}

/// Pattern listings are charged to the definition's folder entries (T08):
/// two folders together past the bound stop the import.
#[test]
fn pattern_listings_share_the_entry_bound() {
    use skin_beatoraja::{ImportError, InputLimit};
    let body = format!(
        "table.insert(skin.source, {{ id = \"p\", path = \"p/*.png\" }})\n\
         table.insert(skin.source, {{ id = \"q\", path = \"q/*.png\" }})\n\
         table.insert(skin.image, {{ id = \"x\", src = \"p\", x = 0, y = 0, w = 8, h = 8 }})\n\
         table.insert(skin.image, {{ id = \"y\", src = \"q\", x = 0, y = 0, w = 8, h = 8 }})\n{}{}",
        dst("x", ""),
        dst("y", "")
    );
    let d = folder(&skin(&body));
    for dir in ["p", "q"] {
        for k in 0..2_100 {
            common::write(d.path(), &format!("{dir}/{k}.txt"), b"");
        }
    }
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let config = Config {
        options: vec![Some(905)],
        ..Config::default()
    };
    assert!(matches!(
        common::lua_play(&h, &config, &cancel),
        Err(ImportError::Input {
            limit: InputLimit::ListEntries,
            ..
        })
    ),);
}

/// A pattern's folder is listed and charged once, however many times the
/// import resolves the pattern: one folder of most of the bound imports.
#[test]
fn a_pattern_folder_is_listed_once() {
    let body = format!(
        "table.insert(skin.source, {{ id = \"p\", path = \"p/*.png\" }})\n\
         table.insert(skin.image, {{ id = \"x\", src = \"p\", x = 0, y = 0, w = 8, h = 8 }})\n{}{}",
        dst("x", ""),
        dst("x", "")
    );
    let d = folder(&skin(&body));
    for k in 0..2_100 {
        common::write(d.path(), &format!("p/{k}.txt"), b"");
    }
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let config = Config {
        options: vec![Some(905)],
        ..Config::default()
    };
    common::lua_play(&h, &config, &cancel).unwrap();
}
