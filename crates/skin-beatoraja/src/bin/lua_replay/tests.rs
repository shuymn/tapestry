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
pub(crate) fn folder(body: &str) -> TempDir {
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

pub(crate) fn options(script: Script) -> Options {
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
        all: true,
        page_point: None,
        deny: Vec::new(),
    }
}

pub(crate) fn run_with(body: &str, o: &Options) -> Report {
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
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(r.compiled.len(), compiled, "{}", r.text(0, true, true));
    assert!(compiled == 0 || r.matched > 0, "{}", r.text(0, true, true));
}

pub(crate) fn dst(id: &str, x: i32, fields: &str) -> String {
    format!(
        "table.insert(skin.destination, {{ id = {id}, {fields} dst = {{ {{ x = {x}, y = 0, w = 64, h = 64 }} }} }})\n"
    )
}

/// A draw only the program runs: the division keeps it out of the
/// expressions.
const HALF_POINT: &str = "function() return ms.number(100) / 2 >= 0 end";

/// The latch: the state clock stored once the point reaches 200 000.
pub(crate) const LATCH: &str = "local latch = (function()\n\
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
    // Three program callbacks a frame (the first `op` skipped), and the
    // third element's lowered draw, compared since e-3.
    assert_eq!(r.matched, r.frames * 4);
}

/// R5: a negative id is a system image, scheduled.
#[test]
fn a_system_image_is_an_object() {
    let body = format!("{LATCH}{}", dst("-111", 0, "timer = latch, loop = -1,"));
    let r = run(&body);
    agrees(&r, 1);
}

/// R6 and §7.1: a negative image-set selector stops the conditions and
/// the timer in the source. Production gates the set's callbacks by the
/// selector, and a latch under that gate is a slot root the program does
/// not run on every play frame: refused, it runs nowhere, and every
/// source invocation is attributed to that record. A control selecting 0
/// through a function is gated and refused the same way; one with no
/// selector (entry 0, never negative) has no gate, and agrees with the
/// latch running.
#[test]
fn a_negative_selector_stops_the_conditions() {
    let set = |value: &str| {
        format!(
            "local latch = (function()\n\
               local state = ms.timer_off_value\n\
               return function()\n\
                 if state == ms.timer_off_value and ms.number(100) > 0 then state = ms.time() end\n\
                 return state\n\
               end\n\
             end)()\n\
             skin.imageset = {{ {{ id = \"s\", images = {{ \"i\", \"j\" }}{value} }} }}\n\
             table.insert(skin.destination, {{ id = \"s\", timer = latch, loop = 2000,\n\
               dst = {{ {{ time = 0, x = 0, y = 0, w = 10, h = 10 }}, {{ time = 2000, x = 100, y = 0, w = 10, h = 10 }} }} }})\n"
        )
    };
    let r = run(&set(
        ", value = function() if ms.number(100) < 100000 then return -1 end return 0 end",
    ));
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(r.production_runs, 0, "{}", r.text(0, true, true));
    assert!(
        r.buckets
            .keys()
            .any(|k| k.contains("a slot writer the program does not run on every play frame")),
        "{}",
        r.text(0, true, true)
    );
    let gated = run(&set(", value = function() return 0 end"));
    assert!(gated.passes(), "{}", gated.text(0, true, true));
    assert_eq!(gated.production_runs, 0, "{}", gated.text(0, true, true));
    let control = run(&set(""));
    agrees(&control, 1);
}

/// §7.1's stateless control: a timer that stores nothing, under the same
/// negative selector, is compiled with the selector as its gate. The
/// source skips it while the selector is negative, production's program
/// skips it on those frames too, and both run it once the point reaches
/// 100 000: the comparison agrees frame by frame.
#[test]
fn a_negative_selector_skips_a_stateless_timer() {
    let body = "skin.imageset = { { id = \"s\", images = { \"i\", \"j\" },\n\
           value = function() if ms.number(100) < 100000 then return -1 end return 0 end } }\n\
         table.insert(skin.destination, { id = \"s\", timer = function() return ms.time() end, loop = 2000,\n\
           dst = { { time = 0, x = 0, y = 0, w = 10, h = 10 }, { time = 2000, x = 100, y = 0, w = 10, h = 10 } } })\n";
    let r = run(body);
    agrees(&r, 1);
    // Skipped on the frames before 100 000 points, run after.
    assert!(r.production_runs > 0, "{}", r.text(0, true, true));
    assert!(
        r.production_runs < 2 * r.frames,
        "{}",
        r.text(0, true, true)
    );
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
        r.text(0, true, true)
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

/// The hi-speed fix (image index 55) a play callback reads agrees on both
/// sides: the reference answers it as the program reads it. Each fix is
/// given to every frame and the draw tests it exactly, so a reference
/// answering any other value draws differently from the program.
#[test]
fn an_index_55_read_agrees() {
    for fix in 1..=4_u8 {
        let mut script = Script::full();
        script.fix_hispeed = fix;
        let r = run_with(
            &dst(
                "\"i\"",
                0,
                &format!("draw = function() return ms.event_index(55) / 2 == {fix} / 2 end,"),
            ),
            &options(script),
        );
        agrees(&r, 1);
    }
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
    let r = s.round_mut(0).unwrap();
    r.presses[0] = Some(999);
    r.extra = vec![pressed + 1];
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
    assert!(!r.findings.is_empty(), "{}", r.text(0, true, true));
}

/// A number's value past `i32`: the point times 1e5, 2e10 at 200 000
/// points.
pub const PAST_I32: &str = "skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,\n\
       value = function() return ms.number(100) * 1e5 end } }\n\
     table.insert(skin.destination, { id = \"v\", dst = { { x = 0, y = 0, w = 64, h = 64 } } })\n";

/// P13: a number's value past `i32` converts to `i32::MAX` on both sides,
/// so nothing is drawn; where the source's `toint()` would keep the low
/// 32 bits (2e10 at 200 000 points: −1 474 836 480), the run counts an
/// intended difference and passes.
#[test]
fn a_value_past_i32_is_an_intended_difference() {
    let r = run(PAST_I32);
    agrees(&r, 1);
    let counted = r
        .intended
        .get(crate::production::compare::TOINT_PAST_I32)
        .copied()
        .unwrap_or(0);
    assert!(counted > 0, "{}", r.text(0, true, true));
    assert!(
        r.text(0, true, true)
            .contains(&format!("intended difference toint past i32: {counted}")),
        "{}",
        r.text(0, true, true)
    );
}

/// A writer stopped by a read the reference cannot answer, and a draw
/// reading what it writes.
const WRITER: &str = "local n\n\
     skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,\n\
       value = function() if ms.float_number(114) ~= 0 then n = 1 else n = 2 end return n end } }\n\
     table.insert(skin.destination, { id = \"i\", draw = function() return n ~= nil end,\n\
       dst = { { x = 0, y = 0, w = 64, h = 64 } } })\n\
     table.insert(skin.destination, { id = \"v\", dst = { { x = 0, y = 0, w = 64, h = 64 } } })\n";

fn site_of(r: &Report) -> u32 {
    r.stopped.first().map_or(0, |s| s.site.1)
}

fn with_isolation(body: &str, text: &str) -> Report {
    let mut o = options(Script::full());
    o.isolation = isolation::parse(text).unwrap();
    run_with(body, &o)
}

/// R16: a stopped writer fails the run unless an entry names it, every
/// cell it writes with its readers, each reader's record matching its
/// disposition and reason.
#[test]
fn stopped_roots_need_reviewed_entries() {
    let bare = run(WRITER);
    assert!(!bare.passes(), "{}", bare.text(0, true, true));
    let line = site_of(&bare);
    assert!(line > 0, "{}", bare.text(0, true, true));
    let reader = line + 1;
    let ok = with_isolation(
        WRITER,
        &format!(
            "root s.luaskin {line} writes n\nreader n s.luaskin {reader} draw replaced Interference\n"
        ),
    );
    assert!(ok.passes(), "{}", ok.text(0, true, true));
    // The reader omitted: the cell has no reviewed readers.
    let omitted = with_isolation(WRITER, &format!("root s.luaskin {line} writes n\n"));
    assert!(!omitted.passes(), "{}", omitted.text(0, true, true));
    assert!(
        omitted
            .stopped
            .iter()
            .any(|s| matches!(&s.covered, Err(e) if e.contains("no reviewed readers"))),
        "{}",
        omitted.text(0, true, true)
    );
    // The wrong disposition, and the wrong reason.
    for wrong in ["dropped", "replaced Writes"] {
        let r = with_isolation(
            WRITER,
            &format!("root s.luaskin {line} writes n\nreader n s.luaskin {reader} draw {wrong}\n"),
        );
        assert!(!r.passes(), "{wrong}: {}", r.text(0, true, true));
    }
    // A reader that is not there.
    let missing = with_isolation(
        WRITER,
        &format!(
            "root s.luaskin {line} writes n\nreader n s.luaskin {} draw replaced Interference\n",
            reader + 50
        ),
    );
    assert!(!missing.passes(), "{}", missing.text(0, true, true));
}

/// R16: a write-free stopped root passes with an empty write set.
#[test]
fn a_write_free_entry_passes() {
    let body = dst(
        "\"i\"",
        0,
        "draw = function() return ms.float_number(114) ~= 0 end,",
    );
    let bare = run(&body);
    assert!(!bare.passes(), "{}", bare.text(0, true, true));
    let r = with_isolation(
        &body,
        &format!("root s.luaskin {} writes\n", site_of(&bare)),
    );
    assert!(r.passes(), "{}", r.text(0, true, true));
}

/// R22 and s7-06e-design.md §7.3: the source's installation removes a
/// number under a disabled option, an absent option, without a keyframe,
/// and under a named static condition that is false (`table_song`
/// outside a table), so it never reads their values; production's
/// installation pass removes the same objects and compiles none of their
/// values. A number under the selected option, or under the negated
/// static condition, is installed and agrees on every frame.
#[test]
fn pruning_invokes_nothing() {
    let v = "value = function() return ms.number(100) / 2 end";
    let body = format!(
        "skin.value = {{ {{ id = \"m\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2, {v} }} }}\n\
         table.insert(skin.destination, {{ id = \"m\", op = {{ 906 }}, dst = {{ {{ x = 0, y = 0, w = 6, h = 6 }} }} }})\n\
         table.insert(skin.destination, {{ id = \"m\", op = {{ -950 }}, dst = {{ {{ x = 0, y = 0, w = 6, h = 6 }} }} }})\n\
         table.insert(skin.destination, {{ id = \"m\", dst = {{}} }})\n\
         table.insert(skin.destination, {{ id = \"m\", op = {{ \"table_song\" }}, dst = {{ {{ x = 0, y = 0, w = 6, h = 6 }} }} }})\n"
    );
    let r = run(&body);
    assert_eq!(r.removed.len(), 4, "{}", r.text(0, true, true));
    assert_eq!(r.points, 0, "{}", r.text(0, true, true));
    assert!(r.compiled.is_empty(), "{}", r.text(0, true, true));
    assert!(r.passes(), "{}", r.text(0, true, true));
    let selected = format!(
        "{body}table.insert(skin.destination, {{ id = \"m\", op = {{ 905 }}, dst = {{ {{ x = 0, y = 0, w = 6, h = 6 }} }} }})\n\
         table.insert(skin.destination, {{ id = \"m\", op = {{ \"!table_song\" }}, dst = {{ {{ x = 0, y = 0, w = 6, h = 6 }} }} }})\n"
    );
    let r = run(&selected);
    assert_eq!(r.removed.len(), 4, "{}", r.text(0, true, true));
    assert_eq!(r.matched, 2 * r.frames, "{}", r.text(0, true, true));
    assert_eq!(r.agreed, 2 * r.frames, "{}", r.text(0, true, true));
}

/// R22 (B1) and s7-06e-design.md §7.3: an image whose declared source
/// does not load is removed by the source's installation, so its draw is
/// never invoked. Production's installation pass claims removal only on
/// evidence it sees without reading a file, so it keeps the compiled draw
/// and runs it every frame (design.md "Differences from the source",
/// play program schedule: noted at the callback, never drawn), found as
/// invocations with no source point. A movie source makes the object
/// unmodelled: its compiled draw is one finding.
#[test]
fn an_unloadable_image_is_removed() {
    let body = format!(
        "table.insert(skin.source, {{ id = \"gone\", path = \"gone.png\" }})
         table.insert(skin.source, {{ id = \"mov\", path = \"m.mp4\" }})
         table.insert(skin.image, {{ id = \"g\", src = \"gone\", x = 0, y = 0, w = 8, h = 8 }})
         table.insert(skin.image, {{ id = \"mv\", src = \"mov\", x = 0, y = 0, w = 8, h = 8 }})
{}{}",
        dst("\"g\"", 0, &format!("draw = {HALF_POINT},")),
        dst("\"mv\"", 0, &format!("draw = {HALF_POINT},"))
    );
    let r = run(&body);
    assert!(
        r.removed.iter().any(|l| l.what == "invalid"),
        "{}",
        r.text(0, true, true)
    );
    assert!(
        r.unmodelled.iter().any(|l| l.what == "movie"),
        "{}",
        r.text(0, true, true)
    );
    assert_eq!(r.agreed, 0, "{}", r.text(0, true, true));
    let count = |s: &str| r.findings.iter().filter(|f| f.what.contains(s)).count();
    assert_eq!(
        count("production invocation"),
        r.frames,
        "{}",
        r.text(0, true, true)
    );
    assert_eq!(
        count("compiled on a unmodelled object (movie)"),
        1,
        "{}",
        r.text(0, true, true)
    );
}

/// R23: a plain image ignores `ref` and `value`; an image set's numeric
/// `value` overrides a conflicting `ref`, a named one (`"point"`) too, a
/// numeric one the factory does not know falls back to `ref`, and a
/// missing `ref` selects 0; a numeric `draw` is a condition; a hole
/// chosen by the selector runs the conditions and the timer, and the
/// object is not sampled.
#[test]
fn constructors_choose_by_their_own_fields() {
    let body = format!(
        "skin.image[1].ref = 42\n\
         skin.imageset = {{ {{ id = \"s\", images = {{ \"i\", \"j\" }}, value = 100, ref = 40 }},\n\
           {{ id = \"p\", images = {{ \"i\", \"j\" }}, value = \"point\", ref = 40 }},\n\
           {{ id = \"u\", images = {{ \"i\", \"j\" }}, value = 99999 }},\n\
           {{ id = \"d\", images = {{ \"i\", \"j\" }} }} }}\n{}{}{}{}{}{}",
        dst("\"i\"", 0, &format!("draw = {HALF_POINT},")),
        dst("\"s\"", 100, &format!("draw = {HALF_POINT},")),
        dst("\"p\"", 200, &format!("draw = {HALF_POINT},")),
        dst("\"u\"", 300, &format!("draw = {HALF_POINT},")),
        dst("\"d\"", 400, &format!("draw = {HALF_POINT},")),
        dst(
            "\"i\"",
            500,
            &format!("op = {{ {HALF_POINT} }}, draw = 32,")
        )
    );
    let r = run(&body);
    agrees(&r, 6);
    assert_eq!(r.matched, r.frames * 6, "{}", r.text(0, true, true));
}

/// R23 and B4: an image set whose selected entry is a hole runs its
/// conditions and its timer, and is never sampled; production's sampler
/// agrees.
#[test]
fn a_hole_runs_the_timer_and_draws_nothing() {
    let body = format!(
        "{LATCH}table.insert(skin.source, {{ id = \"gone\", path = \"gone.png\" }})\n\
         table.insert(skin.image, {{ id = \"g\", src = \"gone\", x = 0, y = 0, w = 8, h = 8 }})\n\
         skin.imageset = {{ {{ id = \"h\", images = {{ \"g\", \"i\" }}, value = function() return ms.number(100) * 0 end }} }}\n{}",
        dst("\"h\"", 0, "timer = latch, loop = -1,")
    );
    let r = run(&body);
    assert!(
        r.findings.iter().all(|f| !f.what.contains("sampled")),
        "{}",
        r.text(0, true, true)
    );
    assert!(r.passes(), "{}", r.text(0, true, true));
}

/// Identity: two destinations of one number definition are two value
/// occurrences; the same function twice in one `op` list is two `op`
/// occurrences.
#[test]
fn occurrences_are_told_apart() {
    let body = format!(
        "skin.value = {{ {{ id = \"m\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,\n\
           value = function() return ms.number(100) / 2 end }} }}\n\
         local f = {HALF_POINT}\n{}{}{}",
        dst("\"m\"", 0, ""),
        dst("\"m\"", 100, ""),
        dst("\"i\"", 200, "op = { f, f },")
    );
    let r = run(&body);
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert!(r.compiled.len() >= 2, "{}", r.text(0, true, true));
}

/// R17: an object of an unmodelled kind lists every callable it holds,
/// its nested parts' included.
#[test]
fn nested_parts_are_listed() {
    let body = "skin.judge = { { id = \"jd\", index = 0, images = {\n\
           { id = \"i\", draw = function() return ms.number(100) / 2 >= 0 end, dst = { { x = 0, y = 0, w = 6, h = 6 } } } },\n\
           numbers = {} } }\n\
         table.insert(skin.destination, { id = \"jd\", draw = function() return true end, dst = { { x = 0, y = 0, w = 6, h = 6 } } })\n";
    let r = run(body);
    let judge = r
        .unmodelled
        .iter()
        .find(|l| l.what == "judge")
        .unwrap_or_else(|| panic!("{}", r.text(0, true, true)));
    assert_eq!(judge.callables.len(), 2, "{}", r.text(0, true, true));
}

/// The isolation file's lines.
#[test]
fn isolation_entries_parse() {
    let e: Vec<Entry> = isolation::parse(
        "# c\nroot a.lua 3 writes x y\nreader x a.lua 5 draw replaced Interference\nreader y none\nroot b.lua 9 writes\n",
    )
    .unwrap();
    assert_eq!(e.len(), 2);
    assert_eq!(e[0].writes, ["x", "y"]);
    assert_eq!(e[0].readers.len(), 1);
    assert_eq!(e[0].readers[0].reason, "Interference");
    assert_eq!(e[0].unread, ["y"]);
    assert!(e[1].writes.is_empty());
    for bad in [
        "reader x a.lua 5 draw replaced Interference\n",
        "root a.lua 3 writes x\nreader z a.lua 5 draw dropped\n",
        "root a.lua 3 writes x\nreader x a.lua 5 draw kept\n",
        "root a.lua 3 writes x\nreader x a.lua 5 draw replaced\n",
        "root a.lua 3 writes x\nreader x a.lua 5 draw dropped Writes\n",
    ] {
        assert!(isolation::parse(bad).is_err(), "{bad}");
    }
    assert!(isolation::same_file("@C:/x/play/play.lua", "play/play.lua"));
    assert!(!isolation::same_file("@C:/x/play/xplay.lua", "play.lua"));
}

/// A3: a property skips a compiled condition (`Gate`), and the timer
/// after it follows that condition, which did not run (`After`).
#[test]
fn a_skipped_condition_skips_the_timer_after_it() {
    let body = dst(
        "\"i\"",
        0,
        &format!(
            "op = {{ 33, {HALF_POINT} }}, timer = function() return ms.time() end, loop = -1,"
        ),
    );
    let r = run(&body);
    agrees(&r, 2);
    assert_eq!(r.matched, r.frames * 2, "{}", r.text(0, true, true));
}

/// B2: a numeric `draw` the boolean-property factory does not know (950,
/// and the configured option 905) adds no condition in the source: the
/// timer after it runs every frame on both sides.
#[test]
fn a_numeric_draw_the_factory_does_not_know_adds_no_condition() {
    let t = "timer = function() return ms.time() end, loop = -1,";
    let body = format!(
        "{}{}",
        dst("\"i\"", 0, &format!("draw = 950, {t}")),
        dst("\"i\"", 100, &format!("draw = 905, {t}"))
    );
    let r = run(&body);
    agrees(&r, 2);
    assert_eq!(r.matched, r.frames * 4, "{}", r.text(0, true, true));
}

/// B2: a grouped image with fewer cells than groups (`divx = divy = 1`,
/// `len = 2`) builds empty arrays, fails validation and is removed: the
/// reference invokes none of its callbacks.
#[test]
fn a_group_with_empty_arrays_is_removed() {
    let body = format!(
        "table.insert(skin.image, {{ id = \"g\", src = \"a\", x = 0, y = 0, w = 8, h = 8, len = 2, ref = 0 }})\n{}",
        dst("\"g\"", 0, &format!("draw = {HALF_POINT},"))
    );
    let r = run(&body);
    assert!(
        r.removed.iter().any(|l| l.what == "invalid"),
        "{}",
        r.text(0, true, true)
    );
    assert_eq!(r.points, 0, "{}", r.text(0, true, true));
    // Production keeps no callback of it either.
    assert!(r.passes(), "{}", r.text(0, true, true));
}

/// B2: an image set whose selected entry's source fact is unknown (a
/// glob) is never taken as a hole: sampling is undecided, a finding. Its
/// timer is stateless: a latch under the set's selector gate is refused.
#[test]
fn an_unknown_entry_is_inconclusive() {
    let body = format!(
        "local latch = function() return ms.time() end\n\
         table.insert(skin.source, {{ id = \"glob\", path = \"x/*.png\" }})\n\
         table.insert(skin.image, {{ id = \"u\", src = \"glob\", x = 0, y = 0, w = 8, h = 8 }})\n\
         skin.imageset = {{ {{ id = \"h\", images = {{ \"i\", \"u\" }}, value = function() return ms.number(100) * 0 + 1 end }} }}\n{}",
        dst("\"h\"", 0, "timer = latch, loop = -1,")
    );
    let r = run(&body);
    assert!(
        r.findings
            .iter()
            .any(|f| f.what.contains("fact is unknown")),
        "{}",
        r.text(0, true, true)
    );
}

/// A4: a listed object's callables keep their consumer: an unmodelled
/// slider's lowered `value` is attributed to production's record of it,
/// and a compiled one is one located finding for the run.
#[test]
fn listed_consumers_join_their_records() {
    let slider = |value: &str| {
        format!(
            "skin.slider = {{ {{ id = \"sl\", src = \"a\", x = 0, y = 0, w = 8, h = 8, angle = 0, range = 100,\n\
               value = function() {value} end }} }}\n{}",
            dst("\"sl\"", 0, "")
        )
    };
    let lowered = run(&slider("return ms.number(100) / 2000"));
    let compiled = run(&slider("return ms.number(100) / 3 + 0.5"));
    for r in [&lowered, &compiled] {
        let listed = r.unmodelled.iter().find(|l| l.what == "slider");
        assert!(
            listed.is_some_and(|l| l
                .callables
                .iter()
                .all(|k| k.field == "value" && k.path == "def")),
            "{}",
            r.text(0, true, true)
        );
        let attributed = r
            .buckets
            .keys()
            .any(|b| b.starts_with("lowered on a unmodelled object"));
        let found = r
            .findings
            .iter()
            .filter(|f| f.what.starts_with("compiled on a unmodelled object"))
            .count();
        // Each listed value is either lowered and counted, or compiled and
        // one finding, never per frame.
        assert!(attributed || found == 1, "{}", r.text(0, true, true));
        assert!(found <= 1, "{}", r.text(0, true, true));
        assert!(
            r.findings.iter().all(|f| f.what.starts_with("compiled on")),
            "{}",
            r.text(0, true, true)
        );
    }
}

/// A4: an isolation reader on a listed object is found by its consumer:
/// a slider's `value` reading what a stopped value writes.
#[test]
fn a_listed_reader_carries_its_evidence() {
    let body = "local n\n\
         skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,\n\
           value = function() if ms.float_number(114) ~= 0 then n = 1 else n = 2 end return n end } }\n\
         skin.slider = { { id = \"sl\", src = \"a\", x = 0, y = 0, w = 8, h = 8, angle = 0, range = 100,\n\
           value = function() return n or 0 end } }\n\
         table.insert(skin.destination, { id = \"sl\", dst = { { x = 0, y = 0, w = 8, h = 8 } } })\n\
         table.insert(skin.destination, { id = \"v\", dst = { { x = 0, y = 0, w = 64, h = 64 } } })\n";
    let bare = run(body);
    let line = site_of(&bare);
    assert!(line > 0, "{}", bare.text(0, true, true));
    let ok = with_isolation(
        body,
        &format!(
            "root s.luaskin {line} writes n\nreader n s.luaskin {} value replaced Interference\n",
            line + 2
        ),
    );
    assert!(ok.passes(), "{}", ok.text(0, true, true));
}

/// B (r3): every integer field the source interprets goes through its
/// serializer's `toint()` (LuaSkinLoader.java:103-104, 144-146): an `op`
/// entry `906.5` is option 906, disabled, so the number is removed
/// (R22); a group of `len = 2` with `divx = 2.5` has 2 cells and is kept,
/// with `divx = 1.5` 1 cell and is removed (truncation, not rounding); a
/// movie is classified before the grid rule (JsonSkinObjectLoader.java:46-49),
/// so a movie with `len = 2` is an unmodelled movie, not removed.
#[test]
fn integer_fields_truncate_and_movies_come_first() {
    let v = "value = function() return ms.number(100) / 2 end";
    let disabled = run(&format!(
        "skin.value = {{ {{ id = \"m\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2, {v} }} }}\n\
         table.insert(skin.destination, {{ id = \"m\", op = {{ 906.5 }}, dst = {{ {{ x = 0, y = 0, w = 6, h = 6 }} }} }})\n"
    ));
    assert!(
        disabled.removed.iter().any(|l| l.what == "option"),
        "{}",
        disabled.text(0, true, true)
    );
    assert_eq!(disabled.points, 0, "{}", disabled.text(0, true, true));
    // Production removes it too: no value compiled, none invoked.
    assert!(disabled.passes(), "{}", disabled.text(0, true, true));
    let group = |divx: &str| {
        run(&format!(
            "table.insert(skin.image, {{ id = \"g\", src = \"a\", x = 0, y = 0, w = 8, h = 8, divx = {divx}, divy = 1, len = 2, ref = 0 }})\n{}",
            dst("\"g\"", 0, &format!("draw = {HALF_POINT},"))
        ))
    };
    let kept = group("2.5");
    agrees(&kept, 1);
    assert_eq!(kept.matched, kept.frames, "{}", kept.text(0, true, true));
    let removed = group("1.5");
    assert!(
        removed.removed.iter().any(|l| l.what == "invalid"),
        "{}",
        removed.text(0, true, true)
    );
    assert_eq!(removed.points, 0, "{}", removed.text(0, true, true));
    let movie = run(&format!(
        "table.insert(skin.source, {{ id = \"mov\", path = \"m.mp4\" }})\n\
         table.insert(skin.image, {{ id = \"mv\", src = \"mov\", x = 0, y = 0, w = 8, h = 8, len = 2 }})\n{}",
        dst("\"mv\"", 0, &format!("draw = {HALF_POINT},"))
    ));
    assert!(
        movie.unmodelled.iter().any(|l| l.what == "movie") && movie.removed.is_empty(),
        "{}",
        movie.text(0, true, true)
    );
}

/// A judge object of parts `images` (after `pre`), with `f` a draw only
/// the program runs.
fn judge(pre: &str, images: &str) -> String {
    format!(
        "local f = {HALF_POINT}\n{pre}\n\
         skin.judge = {{ {{ id = \"jd\", index = 0, images = {{ {images} }}, numbers = {{}} }} }}\n\
         table.insert(skin.destination, {{ id = \"jd\", draw = function() return true end, dst = {{ {{ x = 0, y = 0, w = 6, h = 6 }} }} }})\n"
    )
}

const PART: &str = "{ id = \"i\", draw = f, dst = { { x = 0, y = 0, w = 6, h = 6 } } }";

/// The keys a listed judge holds, and the compiled findings on it.
fn judge_keys(r: &Report) -> (Vec<crate::observed::Key>, usize) {
    let judge = r
        .unmodelled
        .iter()
        .find(|l| l.what == "judge")
        .unwrap_or_else(|| panic!("{}", r.text(0, true, true)));
    let found = r
        .findings
        .iter()
        .filter(|f| {
            f.what
                .starts_with("compiled on a unmodelled object (judge)")
        })
        .count();
    // Every compiled callback joins a listed key: no finding but the one
    // per compiled callable.
    assert!(
        r.findings
            .iter()
            .all(|f| f.what.starts_with("compiled on a unmodelled object")),
        "{}",
        r.text(0, true, true)
    );
    for (_, key, _) in &r.compiled {
        assert!(
            judge.callables.contains(key),
            "{key:?}: {}",
            r.text(0, true, true)
        );
    }
    (judge.callables.clone(), found)
}

/// A4 (r3): two judge parts sharing one draw closure are two occurrences,
/// told apart by their object paths on both sides; production's compiled
/// callbacks join them one each.
#[test]
fn judge_parts_sharing_a_draw_are_two_occurrences() {
    let r = run(&judge("", &format!("{PART}, {PART}")));
    let (keys, found) = judge_keys(&r);
    let paths: Vec<&str> = keys
        .iter()
        .filter(|k| k.field == "draw")
        .map(|k| k.path)
        .collect();
    assert_eq!(
        paths,
        ["", "def.images.1", "def.images.2"],
        "{}",
        r.text(0, true, true)
    );
    assert_eq!(found, 2, "{}", r.text(0, true, true));
}

/// A4 (r3): one part table met twice is two occurrences (a table is
/// skipped only on the way down, as a cycle).
#[test]
fn a_shared_part_table_is_two_occurrences() {
    let r = run(&judge(&format!("local part = {PART}"), "part, part"));
    let (keys, found) = judge_keys(&r);
    let paths: Vec<&str> = keys.iter().map(|k| k.path).collect();
    assert_eq!(
        paths,
        ["", "def.images.1", "def.images.2"],
        "{}",
        r.text(0, true, true)
    );
    assert_eq!(found, 2, "{}", r.text(0, true, true));
}

/// A4 (r3): an `op` entry of a nested part keeps its position in its own
/// list and its part's path on both sides.
#[test]
fn a_nested_op_keeps_its_index_and_path() {
    let r = run(&judge(
        "",
        "{ id = \"i\", op = { 32, f }, dst = { { x = 0, y = 0, w = 6, h = 6 } } }",
    ));
    let (keys, found) = judge_keys(&r);
    assert!(
        keys.iter()
            .any(|k| k.field == "op" && k.index == 1 && k.path == "def.images.1"),
        "{keys:?}"
    );
    assert_eq!(found, 1, "{}", r.text(0, true, true));
}

/// A fixture's production run and what the reference observed, apart, for
/// the comparator's own tests.
pub fn pieces(body: &str) -> (crate::production::ProductionRun, crate::observed::Observed) {
    pieces_over(body, Script::full())
}

/// [`pieces`] over `script`.
pub fn pieces_over(
    body: &str,
    script: Script,
) -> (crate::production::ProductionRun, crate::observed::Observed) {
    let d = folder(body);
    let o = options(script);
    let cancel = skin_beatoraja::Cancel::new();
    let h = skin_beatoraja::read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let config = skin_beatoraja::Config::named(h.header(), [], []).unwrap();
    let (out, rec) = h.import_recorded(&config, &o.facts, &cancel);
    let skin_beatoraja::LuaOutcome::Imported(i) = out.unwrap() else {
        panic!("pending");
    };
    let (run, inputs) =
        crate::production::record(&i.imported.skin, rec, &o.script, Some(12)).unwrap();
    let mut obs = crate::reference::run_rounds(
        &inputs,
        || {
            let (_, rec, vm) = h.import_reference(&config, &o.facts, &cancel);
            let vm = vm.unwrap();
            Ok(crate::reference::Fresh {
                vm,
                items: rec.items,
                sources: rec.sources,
            })
        },
        None,
    )
    .unwrap();
    obs.root = d.path().to_path_buf();
    (run, obs)
}

/// The latch on two destinations, a lowered draw, and a program draw its
/// gate always skips (autoplay on, 33, is false).
pub const TAMPERED: &str = "local latch = (function()\n\
       local state = ms.timer_off_value\n\
       return function()\n\
         if state == ms.timer_off_value and ms.number(100) == 200000 then state = ms.time() end\n\
         return state\n\
       end\n\
     end)()\n\
     table.insert(skin.destination, { id = \"i\", timer = latch, loop = -1, dst = { { x = 0, y = 0, w = 64, h = 64 } } })\n\
     table.insert(skin.destination, { id = \"i\", timer = latch, loop = -1, dst = { { x = 0, y = 70, w = 64, h = 64 } } })\n\
     table.insert(skin.destination, { id = \"i\", draw = function() return ms.number(100) >= 0 end, dst = { { x = 100, y = 0, w = 64, h = 64 } } })\n\
     table.insert(skin.destination, { id = \"i\", op = { 33 }, draw = function() return ms.number(100) / 2 >= 0 end, dst = { { x = 200, y = 0, w = 64, h = 64 } } })\n";
