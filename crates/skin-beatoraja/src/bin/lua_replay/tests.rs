//! The harness on self-made skins (s7-06e-design.md §5.2): each runs both
//! imports, the production frames, the reference and the comparator.

use std::io::Write as _;
use std::path::Path;

use skin_beatoraja::lua::facts::{Fact, ImportFacts, JudgeClass};
use skin_model::ScreenKind;
use tempfile::TempDir;

use crate::isolation::{self, Entry};
use crate::report::Report;
use crate::script::{STATE_AT_ZERO, Script};
use crate::{Options, Stop, replay};

fn png(dir: &Path, name: &str, w: u32, h: u32) {
    let f = std::fs::File::create(dir.join(name)).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let mut w8 = e.write_header().unwrap();
    w8.write_image_data(&vec![255; (w * h * 4) as usize])
        .unwrap();
}

/// A play skin folder whose configured pass runs `body`, with `ms` the
/// host module, an option `fc` (905 off, the default; 906 on), source
/// `a` (64×64), images `i` and `j`.
fn folder(body: &str) -> TempDir {
    let d = tempfile::tempdir().unwrap();
    let text = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080,\n\
           property = {{ {{ name = \"fc\", item = {{ {{ name = \"off\", op = 905 }}, {{ name = \"on\", op = 906 }} }} }} }} }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }},\n\
           {{ id = \"j\", src = \"a\", x = 0, y = 0, w = 32, h = 32 }} }}\n\
         skin.destination = {{}}\n\
         {body}\n\
         return skin\n"
    );
    std::fs::File::create(d.path().join("s.luaskin"))
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    png(d.path(), "a.png", 64, 64);
    d
}

fn options(script: Script) -> Options {
    let mut facts = ImportFacts::unavailable(ScreenKind::Play);
    facts.autoplay = Fact::Known(false);
    facts.judge_class = Fact::Known(Some(JudgeClass::Normal));
    facts.play_level = Fact::Known(12);
    Options {
        facts,
        options: Vec::new(),
        files: Vec::new(),
        script,
        perturb: Vec::new(),
        isolation: Vec::new(),
        local: true,
    }
}

fn run_with(body: &str, o: &Options) -> Report {
    let d = folder(body);
    match replay(d.path(), "s.luaskin", o) {
        Ok(r) => r,
        Err(Stop::Failed(e)) => panic!("failed: {e}"),
        Err(Stop::Differs) => panic!("the imports differ"),
    }
}

fn run(body: &str) -> Report {
    run_with(body, &options(Script::full()))
}

/// Asserts `r` passes, with `compiled` program callbacks, each compared.
fn agrees(r: &Report, compiled: usize) {
    assert!(r.passes(), "{}", r.text(0, true));
    assert_eq!(r.compiled.len(), compiled, "{}", r.text(0, true));
    assert!(compiled == 0 || r.matched > 0, "{}", r.text(0, true));
}

fn dst(id: &str, x: i32, fields: &str) -> String {
    format!(
        "table.insert(skin.destination, {{ id = {id}, {fields} dst = {{ {{ x = {x}, y = 0, w = 64, h = 64 }} }} }})\n"
    )
}

/// A draw only the program runs: the division keeps it out of the
/// expressions.
const HALF_POINT: &str = "function() return ms.number(100) / 2 >= 0 end";

/// The latch: the state clock stored once the point reaches 200 000.
const LATCH: &str = "local latch = (function()\n\
       local state = ms.timer_off_value\n\
       return function()\n\
         if state == ms.timer_off_value and ms.number(100) == 200000 then state = ms.time() end\n\
         return state\n\
       end\n\
     end)()\n";

/// R1: two destinations sharing a closure are two occurrences, each
/// matched to its own program callback.
#[test]
fn shared_closures_are_two_occurrences() {
    let body = format!(
        "local d = {HALF_POINT}\n{}{}",
        dst("\"i\"", 0, "draw = d,"),
        dst("\"i\"", 100, "draw = d,")
    );
    let r = run(&body);
    agrees(&r, 2);
    assert_eq!(r.matched, r.frames * 2);
}

/// R2: an element whose timer is off still runs its draw.
#[test]
fn a_timer_off_element_still_runs_its_draw() {
    let r = run(&dst(
        "\"i\"",
        0,
        &format!("timer = 3, draw = {HALF_POINT},"),
    ));
    agrees(&r, 1);
    assert_eq!(r.matched, r.frames);
}

/// R3: a sentinel value skips its draw on both sides; a negative value
/// without a negative image set runs it.
#[test]
fn a_sentinel_value_skips_its_conditions() {
    let body = format!(
        "skin.value = {{ {{ id = \"m\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,\n\
           value = function() return ms.number(100) * 0 + 2147483647.5 end }},\n\
           {{ id = \"n\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,\n\
           value = function() return ms.number(100) * 0 - 5.5 end }} }}\n{}{}",
        dst("\"m\"", 0, &format!("draw = {HALF_POINT},")),
        dst("\"n\"", 100, &format!("draw = {HALF_POINT},"))
    );
    let r = run(&body);
    agrees(&r, 4);
}

/// R4: the conditions run in the source's order, the first false
/// stopping the rest. Each `op` function is a program callback (one per
/// element); each `draw` lowers, and gates the reference's schedule.
#[test]
fn conditions_run_in_the_source_order() {
    let f = |b: bool| {
        format!(
            "function() return ms.number(100) / 2 >= {} end",
            if b { 0 } else { 1_000_000 }
        )
    };
    let draw = "draw = function() return ms.number(100) >= 0 end,";
    let body = format!(
        "{}{}{}",
        // autoplay_on (33) is false: the function does not run.
        dst("\"i\"", 0, &format!("op = {{ 33, {} }}, {draw}", f(true))),
        // The function is false: the draw does not run.
        dst("\"i\"", 100, &format!("op = {{ {} }}, {draw}", f(false))),
        // All true: the function and the draw run.
        dst("\"i\"", 200, &format!("op = {{ 32, {} }}, {draw}", f(true)))
    );
    let r = run(&body);
    agrees(&r, 3);
    assert_eq!(r.matched, r.frames * 3);
}

/// R5: a negative id is a system image, scheduled.
#[test]
fn a_system_image_is_an_object() {
    let body = format!("{LATCH}{}", dst("-111", 0, "timer = latch, loop = -1,"));
    let r = run(&body);
    agrees(&r, 1);
}

/// R6 and §7.1: a negative image-set selector stops the conditions and
/// the timer in the source; production invokes the latch anyway, a
/// finding until the production fix. The control, selector 0, agrees.
#[test]
fn a_negative_selector_stops_the_conditions() {
    let set = |sel: &str| {
        format!(
            "{LATCH}skin.imageset = {{ {{ id = \"s\", images = {{ \"i\", \"j\" }}, value = function() {sel} end }} }}\n\
             table.insert(skin.destination, {{ id = \"s\", timer = latch, loop = 2000,\n\
               dst = {{ {{ time = 0, x = 0, y = 0, w = 10, h = 10 }}, {{ time = 2000, x = 100, y = 0, w = 10, h = 10 }} }} }})\n"
        )
    };
    let r = run(&set(
        "if ms.number(100) < 200000 then return -1 end return 0",
    ));
    assert!(!r.passes(), "{}", r.text(0, true));
    assert!(
        r.findings
            .iter()
            .any(|f| f.what.contains("not in the source")),
        "{}",
        r.text(0, true)
    );
    let control = run(&set("return 0"));
    agrees(&control, 1);
}

/// R7: a threshold flips on the frame it is crossed, on both sides.
#[test]
fn a_threshold_flips_on_one_frame() {
    let r = run(&dst(
        "\"i\"",
        0,
        "draw = function() return ms.number(100) / 1 >= 100000 end,",
    ));
    agrees(&r, 1);
}

/// R8: zero divisors and negative integer modulo agree, raw and
/// converted.
#[test]
fn zero_divisors_and_negative_modulo() {
    let body = format!(
        "skin.value = {{ {{ id = \"z\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,\n\
           value = function() return ms.number(110) / ms.number(111) end }},\n\
           {{ id = \"q\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,\n\
           value = function() return 1 / ms.number(111) end }},\n\
           {{ id = \"r\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,\n\
           value = function() return (ms.number(110) - 7) % 3 + ms.number(100) / 1000000 end }} }}\n{}{}{}",
        dst("\"z\"", 0, ""),
        dst("\"q\"", 100, ""),
        dst("\"r\"", 200, "")
    );
    agrees(&run(&body), 3);
}

/// R9: a case where Lua 5.4 and LuaJ differ is a finding: a negated zero
/// count divides to −inf in Lua 5.4 and to +inf in the source, which the
/// program follows.
#[test]
fn luaj_differences_are_findings() {
    let body = format!(
        "skin.value = {{ {{ id = \"z\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,\n\
           value = function() return 1 / (-ms.number(111)) end }} }}\n{}",
        dst("\"z\"", 0, "")
    );
    let r = run(&body);
    assert!(
        r.findings
            .iter()
            .any(|f| f.what.starts_with("raw") && f.what.contains("luaj-operator: / unary -")),
        "{}",
        r.text(0, true)
    );
}

/// R11: a read whose guard is false is unavailable on both sides.
#[test]
fn an_unavailable_read_agrees() {
    // The hi-speed (10) is unavailable: the frames give no readouts.
    let r = run(&dst(
        "\"i\"",
        0,
        "draw = function() return ms.number(10) / 2 >= 0 end,",
    ));
    agrees(&r, 1);
}

/// R12: a latch agrees on every frame, both invocations, start, elapsed,
/// installed start and sampled age; a latch returning its old value on
/// two destinations agrees too.
#[test]
fn a_latch_agrees_across_frames() {
    let body = format!(
        "{LATCH}{}{}",
        dst("\"i\"", 0, "timer = latch, loop = -1,"),
        dst("\"i\"", 100, "timer = latch, loop = -1,")
    );
    agrees(&run(&body), 2);
    let old = "local latch = (function()\n\
           local state = ms.timer_off_value\n\
           return function()\n\
             local was = state\n\
             if state == ms.timer_off_value and ms.number(100) == 200000 then state = ms.time() end\n\
             return was\n\
           end\n\
         end)()\n";
    let body = format!(
        "{old}{}{}",
        dst("\"i\"", 0, "timer = latch, loop = -1,"),
        dst("\"i\"", 100, "timer = latch, loop = -1,")
    );
    agrees(&run(&body), 2);
}

/// R13: a start with a sub-ms remainder: the latch sets at the first
/// judgment, pressed 999 µs late (a state clock of …999 µs), and the
/// frame 1 µs later is 1 ms old by the source's two divisions, where
/// subtracting first would give 0.
#[test]
fn a_timer_age_with_a_sub_ms_start_agrees() {
    let latch = "local latch = (function()\n\
           local state = ms.timer_off_value\n\
           return function()\n\
             if state == ms.timer_off_value and ms.number(100) > 0 then state = ms.time() end\n\
             return state\n\
           end\n\
         end)()\n";
    let mut s = Script::full();
    let pressed = Script::note_time(0) + 999;
    s.presses[0] = Some(999);
    s.extra = vec![pressed + 1];
    assert_eq!((pressed + STATE_AT_ZERO) % 1_000, 999);
    let mut o = options(s);
    o.local = true;
    let body = format!("{latch}{}", dst("\"i\"", 0, "timer = latch, loop = -1,"));
    let r = run_with(&body, &o);
    agrees(&r, 1);
}

/// R14: perturbing a compiled read is found.
#[test]
fn a_perturbed_read_is_found() {
    let body = format!("{LATCH}{}", dst("\"i\"", 0, "timer = latch, loop = -1,"));
    let mut o = options(Script::full());
    o.perturb = vec![(100, -1)];
    let r = run_with(&body, &o);
    assert!(!r.findings.is_empty(), "{}", r.text(0, true));
}

/// A writer stopped by a read the reference cannot answer, and a draw
/// reading what it writes.
const WRITER: &str = "local n\n\
     skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,\n\
       value = function() if ms.float_number(113) ~= 0 then n = 1 else n = 2 end return n end } }\n\
     table.insert(skin.destination, { id = \"i\", draw = function() return n ~= nil end,\n\
       dst = { { x = 0, y = 0, w = 64, h = 64 } } })\n\
     table.insert(skin.destination, { id = \"v\", dst = { { x = 0, y = 0, w = 64, h = 64 } } })\n";

fn site_of(r: &Report) -> u32 {
    r.stopped.first().map_or(0, |s| s.site.1)
}

/// R16: a stopped writer fails the run unless an entry names it, its
/// write set and every reader with the record it carries.
#[test]
fn stopped_roots_need_reviewed_entries() {
    let bare = run(WRITER);
    assert!(!bare.passes(), "{}", bare.text(0, true));
    let line = site_of(&bare);
    assert!(line > 0, "{}", bare.text(0, true));
    let reader = line + 1;
    let with = |text: &str| {
        let mut o = options(Script::full());
        o.isolation = isolation::parse(text).unwrap();
        run_with(WRITER, &o)
    };
    // Every reader with its record: passes.
    let ok = with(&format!(
        "root s.luaskin {line} writes n\nreader s.luaskin {reader} draw replaced\n"
    ));
    assert!(ok.passes(), "{}", ok.text(0, true));
    // The wrong record: fails.
    let wrong = with(&format!(
        "root s.luaskin {line} writes n\nreader s.luaskin {reader} draw dropped\n"
    ));
    assert!(!wrong.passes(), "{}", wrong.text(0, true));
    // A reader that is not there: fails.
    let missing = with(&format!(
        "root s.luaskin {line} writes n\nreader s.luaskin {} draw replaced\n",
        reader + 50
    ));
    assert!(!missing.passes(), "{}", missing.text(0, true));
}

/// R16: a write-free stopped root passes with an empty write set.
#[test]
fn a_write_free_entry_passes() {
    let body = dst(
        "\"i\"",
        0,
        "draw = function() return ms.float_number(113) ~= 0 end,",
    );
    let bare = run(&body);
    assert!(!bare.passes());
    let mut o = options(Script::full());
    o.isolation = isolation::parse(&format!("root s.luaskin {} writes\n", site_of(&bare))).unwrap();
    let r = run_with(&body, &o);
    assert!(r.passes(), "{}", r.text(0, true));
}

/// R22: the source's installation removes an object under a disabled
/// option, an absent option and without a keyframe: the reference
/// invokes none of its consumers.
#[test]
fn pruning_invokes_nothing() {
    let v = "value = function() return ms.number(100) / 2 end";
    let body = format!(
        "skin.value = {{ {{ id = \"m\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2, {v} }} }}\n\
         table.insert(skin.destination, {{ id = \"m\", op = {{ 906 }}, dst = {{ {{ x = 0, y = 0, w = 6, h = 6 }} }} }})\n\
         table.insert(skin.destination, {{ id = \"m\", op = {{ -950 }}, dst = {{ {{ x = 0, y = 0, w = 6, h = 6 }} }} }})\n\
         table.insert(skin.destination, {{ id = \"m\", dst = {{}} }})\n"
    );
    let r = run(&body);
    assert_eq!(r.removed.len(), 3, "{}", r.text(0, true));
    assert_eq!(r.points, 0, "{}", r.text(0, true));
}

/// R23: a plain image ignores `ref` and `value`; an image set's numeric
/// `value` overrides a conflicting `ref`.
#[test]
fn constructors_choose_by_their_own_fields() {
    let body = format!(
        "skin.image[1].ref = 42\n\
         skin.imageset = {{ {{ id = \"s\", images = {{ \"i\", \"j\" }}, value = 100, ref = 40 }} }}\n{}{}",
        dst("\"i\"", 0, &format!("draw = {HALF_POINT},")),
        dst("\"s\"", 100, &format!("draw = {HALF_POINT},"))
    );
    let r = run(&body);
    agrees(&r, 2);
    assert_eq!(r.matched, r.frames * 2);
}

/// The isolation file's lines.
#[test]
fn isolation_entries_parse() {
    let e: Vec<Entry> = isolation::parse(
        "# c\nroot a.lua 3 writes x y\nreader a.lua 5 draw replaced\nroot b.lua 9 writes\n",
    )
    .unwrap();
    assert_eq!(e.len(), 2);
    assert_eq!(e[0].writes, ["x", "y"]);
    assert_eq!(e[0].readers.len(), 1);
    assert!(e[1].writes.is_empty());
    assert!(isolation::parse("reader a.lua 5 draw replaced\n").is_err());
    assert!(isolation::parse("root a.lua 3 writes\nreader a.lua 5 draw kept\n").is_err());
    assert!(isolation::same_file("@C:/x/play/play.lua", "play/play.lua"));
    assert!(!isolation::same_file("@C:/x/play/xplay.lua", "play.lua"));
}
