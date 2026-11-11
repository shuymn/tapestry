//! The bridges over state the model already holds (docs/stage3.md §3,
//! S7-06f), through the real import and frames: the judgment-count values
//! and conditions, `main_state.judge`, and an image set's value. Every
//! fixture is a small self-made skin.

mod common;

use common::play::{Drawn, Loader, context, round};
use skin_beatoraja::lua::{Disposition, Unsupported};
use skin_beatoraja::{Cancel, Config, ImportError, LuaImported, LuaWhy, NoteKind, read_lua_header};
use skin_model::{GaugeDesc, GaugeLook, LoadingGauge, PlayContext, PlayScore, Quad, ScoreBasis};
use tempfile::TempDir;

/// A skin folder whose `s.luaskin` runs `body` with `ms` the host module,
/// `skin.source` `a` and images `i0` to `i8`, each a cell of its own.
fn folder(body: &str) -> TempDir {
    let d = tempfile::tempdir().unwrap();
    let entry = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{}}\n\
         for k = 0, 8 do\n\
           skin.image[k + 1] = {{ id = \"i\" .. k, src = \"a\", x = k * 4, y = 0, w = 4, h = 4 }}\n\
         end\n\
         skin.destination = {{}}\n\
         {body}\n\
         return skin\n"
    );
    common::write(d.path(), "s.luaskin", entry.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    d
}

fn import(d: &TempDir) -> Result<LuaImported, ImportError> {
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel)?;
    common::lua_play(&h, &Config::default(), &cancel)
}

/// `body`'s skin in a play of `notes` notes, its images read.
fn drawn(body: &str, notes: u32) -> Drawn {
    let d = folder(body);
    let i = import(&d).unwrap();
    Drawn::new(i.imported.skin, notes)
}

/// A skin's end-of-play image on the last-note timer (143), drawn only
/// after a combo break (425 above 0): its window 500 to 1450 ms after the
/// notes' end, opaque from 750 to 1250, read at exact frame times. The
/// first playing frame past the end starts the timer at the end itself,
/// not at that frame: its first frame comes 600 ms late.
#[test]
fn a_finish_window_runs_from_the_notes_end_after_a_break() {
    let body = "skin.destination = { { id = \"i0\", timer = 143, loop = -1,\n\
                  draw = function() return ms.number(425) > 0 end, dst = {\n\
                  { time = 500, x = 0, y = 0, w = 1920, h = 1080, a = 0 },\n\
                  { time = 750, a = 255 }, { time = 1250, a = 255 }, { time = 1450, a = 0 } } } }";
    let d = folder(body);
    let skin = import(&d).unwrap().imported.skin;
    let chart = common::play::chart(1);
    let end = bms_model::TimeUs(1_000_000);
    let alpha = |d: &mut Drawn, ms: i64| d.frame(ms, &[]).first().map(|q| q.color[3]);
    // A miss: the window shows, opaque at 900 ms past the end.
    let mut missed = Drawn::over_ending(skin.clone(), &chart, end);
    missed.tally = counts(0, [1, 0], 0);
    assert_eq!(alpha(&mut missed, 999), None);
    // 600 ms from the end, rising from 500 to 750: 0.4.
    let rising = alpha(&mut missed, 1_600).unwrap();
    assert!((rising - 0.4).abs() < 1e-3, "{rising}");
    assert_eq!(alpha(&mut missed, 1_900), Some(1.0));
    // 1300 ms from the end, falling from 1250 to 1450: 0.75.
    let falling = alpha(&mut missed, 2_300).unwrap();
    assert!((falling - 0.75).abs() < 1e-3, "{falling}");
    assert_eq!(alpha(&mut missed, 2_600), None);
    // A perfect play: nothing at the same time.
    let mut perfect = Drawn::over_ending(skin, &chart, end);
    assert_eq!(alpha(&mut perfect, 1_000), None);
    assert_eq!(alpha(&mut perfect, 1_900), None);
}

/// `body`'s skin while loading, its images read.
fn loader(body: &str) -> Loader {
    let d = folder(body);
    Loader::new(import(&d).unwrap().imported.skin)
}

/// A tally of `bad` BADs, `poor` scored POORs, split early and late, and
/// `empty` empty POORs not counted as POORs.
fn counts(bad: u32, poor: [u32; 2], empty: u32) -> eval::Tally {
    let mut t = common::play::tally(0, 0);
    t.counts.bad = bad;
    t.early.bad = bad;
    t.counts.poor = poor[0] + poor[1];
    t.early.poor = poor[0];
    t.late.poor = poor[1];
    t.empty_poor = empty;
    t.empty_poor_early = empty;
    t
}

/// Images drawn `10 + k` wide while `expr == k`, for each `k` to 12, so a
/// frame's width reads `expr`'s value.
fn reading(expr: &str) -> String {
    (0..=12)
        .map(|k| {
            format!(
                "table.insert(skin.destination, {{ id = \"i0\",\n\
                   draw = function() return {expr} == {k} end,\n\
                   dst = {{ {{ x = 0, y = 0, w = {w}, h = 10 }} }} }})\n",
                w = 10 + k
            )
        })
        .collect()
}

/// What `reading` reads in `d`'s frame.
fn read(d: &mut Drawn) -> Vec<i32> {
    d.rects(1_000).iter().map(|r| r[2] - 10).collect()
}

/// `expr`'s value over `tally`, in a play that counts empty POORs when
/// `counted`.
fn value_of(expr: &str, tally: eval::Tally, counted: bool) -> Vec<i32> {
    let mut d = drawn(&reading(expr), 20);
    if counted {
        let ctx = PlayContext {
            empty_poor_counted: true,
            ..context()
        };
        d.play = d.skin.prepare(&d.chart, &ctx).unwrap();
    }
    d.tally = tally;
    read(&mut d)
}

/// The digits a number of `prop` shows over `tally`, left to right, each
/// glyph told by its uv against an EX score number cut from the same
/// strip, drawn below it.
fn digits(prop: &str, tally: eval::Tally) -> String {
    let mut d = drawn(
        &format!(
            "skin.value = {{\n\
               {{ id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8, {prop} }},\n\
               {{ id = \"ex\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 1, ref = 71 }} }}\n\
             table.insert(skin.destination, {{ id = \"v\", dst = {{ {{ x = 0, y = 900, w = 6, h = 6 }} }} }})\n\
             table.insert(skin.destination, {{ id = \"ex\", dst = {{ {{ x = 0, y = 100, w = 6, h = 6 }} }} }})"
        ),
        20,
    );
    d.tally = tally;
    let glyphs = d.ex_glyphs(1_000);
    let mut shown: Vec<_> = d
        .frame(1_000, &[])
        .into_iter()
        .filter(|q| q.rect[1] < 500.0)
        .collect();
    shown.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
    shown
        .iter()
        .map(|q| glyphs[&q.uv.map(f32::to_bits)])
        .collect()
}

/// 425 is BAD and the scored POORs, 427 those and the empty POORs, by
/// `ref`, by `value` and read in a callback.
#[test]
fn the_combo_break_counts_sum_the_scored_judgments() {
    for prop in ["ref = 425", "value = 425"] {
        assert_eq!(digits(prop, counts(0, [0, 0], 0)), "0", "{prop}");
        assert_eq!(digits(prop, counts(3, [4, 5], 90)), "12", "{prop}");
        assert_eq!(digits(prop, counts(3, [49, 50], 0)), "102", "{prop}");
    }
    for prop in ["ref = 427", "value = 427"] {
        assert_eq!(digits(prop, counts(0, [0, 0], 0)), "0", "{prop}");
        assert_eq!(digits(prop, counts(3, [4, 5], 0)), "12", "{prop}");
        assert_eq!(digits(prop, counts(3, [4, 5], 90)), "102", "{prop}");
    }
    let tally = || counts(3, [4, 5], 90);
    assert_eq!(value_of("ms.number(425)", tally(), false), [12]);
    assert_eq!(
        value_of("ms.number(427)", counts(1, [1, 1], 2), false),
        [5],
        "each term once"
    );
    assert_eq!(digits("ref = 425", counts(0, [0, 0], 5)), "0");
}

/// A sum's range is its terms': arithmetic that could leave `i32` over it
/// is no expression, where the same factor stays inside over one count;
/// within ±2^53 the program takes it.
#[test]
fn a_sums_range_bounds_a_callbacks_arithmetic() {
    let draw = |expr: &str| {
        let dir = folder(&format!(
            "table.insert(skin.destination, {{ id = \"i0\", draw = function() return {expr} > 0 end,\n\
               dst = {{ {{ w = 10, h = 10 }} }} }})"
        ));
        import(&dir).unwrap().callbacks
    };
    assert_eq!(draw("ms.number(113) * 100"), [Disposition::Lowered]);
    for expr in [
        "ms.number(425) * 100",
        "ms.number(427) * 100",
        "ms.judge(4) * 200",
    ] {
        assert_eq!(draw(expr), [Disposition::Compiled], "{expr}");
    }
}

/// One empty POOR alone under a preset that counts it as a POOR: `ref` 114
/// keeps the preset's count, the rest the scored POORs.
#[test]
fn a_counted_empty_poor_is_no_scored_poor() {
    let empty = || {
        let mut t = counts(0, [0, 0], 1);
        t.counts.poor = 1;
        t
    };
    assert_eq!(value_of("ms.number(114)", empty(), true), [1]);
    assert_eq!(value_of("ms.number(425)", empty(), true), [0]);
    assert_eq!(value_of("ms.number(427)", empty(), true), [1]);
    assert_eq!(value_of("ms.judge(4)", empty(), true), [0]);
    assert_eq!(value_of("ms.judge(5)", empty(), true), [1]);
    let mut d = drawn(
        "table.insert(skin.destination, { id = \"i0\", op = { 2245 }, dst = { { w = 10, h = 10 } } })",
        20,
    );
    let ctx = PlayContext {
        empty_poor_counted: true,
        ..context()
    };
    d.play = d.skin.prepare(&d.chart, &ctx).unwrap();
    d.tally = empty();
    assert!(d.rects(1_000).is_empty(), "2245 false");
}

/// Each condition 2242 to 2245 over its own count, and its negation.
#[test]
fn judgment_conditions_read_their_counts() {
    for (op, set) in [
        (2_242, (|t| t.counts.great = 1) as fn(&mut eval::Tally)),
        (2_243, |t| t.counts.good = 1),
        (2_244, |t| t.counts.bad = 1),
        (2_245, |t| t.early.poor = 1),
        (2_245, |t| t.late.poor = 1),
    ] {
        let mut d = drawn(
            &format!(
                "table.insert(skin.destination, {{ id = \"i0\", op = {{ {op} }}, dst = {{ {{ w = 10, h = 10 }} }} }})\n\
                 table.insert(skin.destination, {{ id = \"i0\", op = {{ -{op} }}, dst = {{ {{ w = 11, h = 10 }} }} }})"
            ),
            20,
        );
        let widths = |d: &mut Drawn| d.rects(1_000).iter().map(|r| r[2]).collect::<Vec<_>>();
        d.tally = counts(0, [0, 0], 0);
        assert_eq!(widths(&mut d), [11], "{op} at 0");
        set(&mut d.tally);
        d.tally.counts.poor = d.tally.early.poor + d.tally.late.poor;
        assert_eq!(widths(&mut d), [10], "{op} above 0");
    }
}

/// Callbacks written as the surveyed skins write them: a `finish` image on
/// the last note's timer that a miss turns on, and a missed line.
#[test]
fn callbacks_lower_the_count_bridges() {
    let dir = folder(
        "table.insert(skin.destination, { id = \"i0\", timer = 143, loop = -1, draw = function()\n\
           local cb = ms.number(425) return cb > 0 end,\n\
           dst = { { time = 0, x = 0, y = 0, w = 10, h = 10 }, { time = 100000 } } })\n\
         local bd, pr = 2244, 2245\n\
         table.insert(skin.destination, { id = \"i1\", draw = function()\n\
           return ms.option(bd) or ms.option(pr) end,\n\
           dst = { { x = 0, y = 0, w = 11, h = 10 } } })",
    );
    let i = import(&dir).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered, Disposition::Lowered]);
    let mut d = Drawn::new(i.imported.skin, 20);
    let ctx = PlayContext {
        last_note_end: bms_model::TimeUs(2_000_000),
        ..context()
    };
    d.play = d.skin.prepare(&d.chart, &ctx).unwrap();
    d.tally = counts(0, [0, 0], 3);
    assert!(d.rects(30_000).is_empty(), "empty POORs alone");
    d.tally = counts(0, [0, 1], 0);
    let shown: Vec<_> = d.rects(30_000).iter().map(|r| r[2]).collect();
    assert_eq!(shown, [10, 11]);
}

/// `judge(n)` by index, 0 for an index with no count.
#[test]
fn judge_reads_the_counts_by_index() {
    let mut t = counts(4, [2, 3], 6);
    t.counts.perfect = 1;
    t.counts.great = 2;
    t.counts.good = 3;
    for (n, want) in [
        (0, 1),
        (1, 2),
        (2, 3),
        (3, 4),
        (4, 5),
        (5, 6),
        (6, 0),
        (-1, 0),
    ] {
        assert_eq!(value_of(&format!("ms.judge({n})"), t, false), [want], "{n}");
    }
    let left =
        "ms.number(106) - (ms.judge(0) + ms.judge(1) + ms.judge(2) + ms.judge(3) + ms.judge(4))";
    assert_eq!(value_of(left, t, false), [5], "the shape of notes left");
    // An argument not a known integer: no coercion.
    for arg in ["1.5", "\"1\"", "ms.number(110)"] {
        let i = import(&folder(&format!(
            "table.insert(skin.destination, {{ id = \"i0\", draw = function() return ms.judge({arg}) > 0 end,\n\
               dst = {{ {{ w = 10, h = 10 }} }} }})"
        )))
        .unwrap();
        assert!(
            matches!(i.callbacks[..], [Disposition::Replaced(_)]),
            "{arg}: {:?}",
            i.callbacks
        );
    }
}

/// An image set of `value` drawn at x 0, and each of its images apart, to
/// tell entries by their uv.
fn image_set(value: &str, more: &str) -> String {
    let images: Vec<String> = (0..9).map(|k| format!("\"i{k}\"")).collect();
    let mut s = format!(
        "skin.imageset = {{ {{ id = \"s\", ref = 40, images = {{ {} }}, value = {value} }} }}\n\
         table.insert(skin.destination, {{ id = \"s\", {more} dst = {{ {{ x = 0, y = 0, w = 10, h = 10 }} }} }})\n",
        images.join(", ")
    );
    for k in 0..9 {
        s.push_str(&format!(
            "table.insert(skin.destination, {{ id = \"i{k}\", dst = {{ {{ x = {x}, y = 0, w = 4, h = 4 }} }} }})\n",
            x = 1_000 + 10 * k
        ));
    }
    s
}

/// The entry the set's quad shows among `quads`, if it drew.
fn entry(quads: &[Quad]) -> Option<i32> {
    let set = quads.iter().find(|q| round(q.rect)[2] == 10)?;
    quads
        .iter()
        .find(|q| round(q.rect)[2] == 4 && q.uv == set.uv && q.layer == set.layer)
        .map(|q| (round(q.rect)[0] - 1_000) / 10)
}

/// A bare `gauge_type()` chooses by the shown gauge's look, with its D4
/// note.
#[test]
fn an_image_set_chooses_by_gauge_type() {
    let d = folder(&image_set("function() return ms.gauge_type() end", ""));
    let i = import(&d).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered]);
    assert!(
        i.imported
            .report
            .notes
            .iter()
            .any(|n| matches!(n.kind, NoteKind::Deviation(skin_beatoraja::DeviationId::D4)))
    );
    for (k, look) in GaugeLook::ALL.into_iter().enumerate() {
        let mut p = Drawn::with_look(import(&d).unwrap().imported.skin, 10, look);
        assert_eq!(
            entry(&p.frame(1_000, &[])),
            Some(i32::try_from(k).unwrap()),
            "{look:?}"
        );
    }
}

/// A numeric callback chooses by its value, under its read's guard, and a
/// past-the-end value entry 0 and a negative none, as the source's image.
#[test]
fn an_image_set_chooses_by_its_value() {
    let mut p = drawn(
        &image_set(
            "function() if ms.number(110) > 0 then return 1 else return 0 end end",
            "",
        ),
        10,
    );
    assert_eq!(entry(&p.frame(1_000, &[])), Some(0));
    p.tally.counts.perfect = 1;
    assert_eq!(entry(&p.frame(1_000, &[])), Some(1));

    let dir = folder(&image_set("function() return ms.number(96) end", ""));
    let chart = common::play::chart(10);
    for (level, want) in [
        (None, None),
        (Some(3), Some(3)),
        (Some(40), Some(0)),
        (Some(-1), None),
    ] {
        let skin = import(&dir).unwrap().imported.skin;
        let mut p = Drawn::over_level(skin, &chart, level);
        assert_eq!(entry(&p.frame(1_000, &[])), want, "{level:?}");
    }
}

/// The part a selector's reads show is the element's role through the
/// whole mapping: a valid one lowers, an unsupported dependency refuses
/// it, where a decorative selector's drops with a note.
#[test]
fn an_image_sets_reads_carry_its_role_through_the_mapping() {
    let critical = "function() return ms.number(101) end";
    let dir = folder(&image_set(critical, ""));
    let i = import(&dir).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered]);
    let mut p = Drawn::new(i.imported.skin, 10);
    p.tally.ex_score = 2;
    assert_eq!(entry(&p.frame(1_000, &[])), Some(2));

    let unsupported = "op = { 289 },";
    assert!(matches!(
        import(&folder(&image_set(critical, unsupported))),
        Err(ImportError::Critical { .. })
    ));
    let mut p = drawn(
        &image_set("function() return ms.number(110) end", unsupported),
        10,
    );
    assert_eq!(entry(&p.frame(1_000, &[])), None, "dropped");
}

/// A value that does not lower follows its role, never `ref`: its reason
/// kept, whether decorative or critical. `gauge_type()` in arithmetic is
/// still a type the expressions do not take.
#[test]
fn an_image_set_value_that_does_not_lower_follows_its_role() {
    for (value, why) in [
        (
            "function() return ms.gauge_type() + 1 end",
            Unsupported::Type,
        ),
        ("function() return \"x\" end", Unsupported::Type),
        (
            "function() return ms.number(426) end",
            Unsupported::Unmapped { api: 1, id: 426 },
        ),
    ] {
        let dir = folder(&image_set(value, ""));
        let i = import(&dir).unwrap();
        assert_eq!(i.callbacks, [Disposition::Replaced(why)], "{value}");
        let mut p = Drawn::new(i.imported.skin, 10);
        assert_eq!(entry(&p.frame(1_000, &[])), None, "no ref: {value}");
    }
    match import(&folder(&image_set(
        "function() return ms.number(101) + ms.number(426) end",
        "",
    ))) {
        Err(ImportError::Lua {
            why:
                LuaWhy::Critical {
                    field: "value",
                    why: Unsupported::Unmapped { id: 426, .. },
                },
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
}

/// While loading no program reading a count is evaluated, its negation
/// included; a play frame at zero counts draws that negation. The gauge's
/// selector follows the descriptor.
#[test]
fn loading_reads_no_counts_and_the_selector_follows_the_descriptor() {
    let body = "table.insert(skin.destination, { id = \"i0\", op = { -2244 }, dst = { { w = 10, h = 10 } } })\n\
         table.insert(skin.destination, { id = \"i0\", draw = function() return ms.number(425) == 0 end,\n\
           dst = { { w = 11, h = 10 } } })\n\
         skin.value = { { id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8, ref = 425 } }\n\
         table.insert(skin.destination, { id = \"v\", dst = { { x = 0, y = 100, w = 6, h = 6 } } })";
    let mut l = loader(body);
    assert!(l.rects(100).is_empty(), "while loading");
    let mut d = drawn(body, 10);
    d.tally = counts(0, [0, 0], 0);
    let widths: Vec<_> = d.rects(1_000).iter().map(|r| r[2]).collect();
    assert_eq!(widths, [10, 11, 6], "a play frame at zero counts");

    let mut l = loader(&image_set("function() return ms.gauge_type() end", ""));
    assert_eq!(entry(&l.frame(100)), Some(2), "the normal look");
    let desc = GaugeDesc {
        look: GaugeLook::Hazard,
        ..common::play::GAUGES[0]
    };
    l.gauge = Some(LoadingGauge::of(0, &desc));
    assert_eq!(entry(&l.frame(200)), Some(5));
    l.gauge = None;
    assert_eq!(entry(&l.frame(300)), None, "no descriptor");
}

/// The bridges read each frame's snapshot: its counts, then the zero a
/// rearmed candidate's evaluator starts from, then the new attempt's.
#[test]
fn combo_break_follows_each_snapshot() {
    let mut d = drawn(&reading("ms.number(425)"), 20);
    d.tally = counts(1, [1, 1], 0);
    assert_eq!(read(&mut d), [3]);
    d.play.rearm();
    d.tally = counts(0, [0, 0], 0);
    assert_eq!(read(&mut d), [0]);
    d.tally = counts(1, [0, 1], 0);
    assert_eq!(read(&mut d), [2]);
}

/// s7-06f-design.md F5: a frame-time `main_state.offset` read stays
/// unsupported: a decorative value is replaced by its constant, the reason
/// naming `main_state.offset`, and one that is critical (it reads the EX
/// score) refuses the skin, where the analysis meets the read's table
/// indexed as an unresolved callee.
#[test]
fn a_frame_time_offset_read_follows_its_role() {
    let offset = Unsupported::HostApi(4);
    let i = import(&folder(&image_set(
        "function() return ms.offset(71).w end",
        "",
    )))
    .unwrap();
    assert_eq!(i.callbacks, [Disposition::Replaced(offset)]);
    match import(&folder(&image_set(
        "function() return ms.number(101) + ms.offset(71).w end",
        "",
    ))) {
        Err(ImportError::Lua {
            why:
                LuaWhy::Critical {
                    field: "value",
                    why,
                },
            ..
        }) => assert_eq!(why, Unsupported::Unresolved("callee")),
        other => panic!("{other:?}"),
    }
}

/// The play of s10-10-design.md §8: a chart of 7 notes (2n = 14), the best
/// 11 and the target 13, both read; drawn as `#RANDOM` when `random`.
fn scored(body: &str, random: bool) -> Drawn {
    scored_by(body, random, 11, 13)
}

/// [`scored`] with the best `best` (unread when 0) and the target `target`.
fn scored_by(body: &str, random: bool, best: u32, target: u32) -> Drawn {
    let mut d = drawn(body, 7);
    if random {
        d.chart.identity.random_results = vec![1];
    }
    let ctx = PlayContext {
        score: PlayScore {
            basis: ScoreBasis {
                best,
                best_known: best > 0,
                target,
            },
            ..PlayScore::NONE
        },
        ..context()
    };
    d.play = d.skin.prepare(&d.chart, &ctx).unwrap();
    d
}

/// §8's progress: 3 judged targets (2 PERFECT, 1 GREAT), EX 5.
fn judged_five() -> eval::Tally {
    let mut t = common::play::tally(5, 3);
    t.counts.perfect = 2;
    t.counts.great = 1;
    t
}

/// `ex` EX over `ex / 2` PERFECTs and, for an odd `ex`, one GREAT.
fn judged_ex(ex: u32) -> eval::Tally {
    let mut t = common::play::tally(ex, ex / 2 + ex % 2);
    t.counts.perfect = ex / 2;
    t.counts.great = ex % 2;
    t
}

/// The digits a number of `prop` shows in [`scored`] over `tally`, as
/// [`digits`] reads them.
fn score_digits(prop: &str, tally: eval::Tally, random: bool) -> String {
    let mut d = scored(
        &format!(
            "skin.value = {{\n\
               {{ id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 8, {prop} }},\n\
               {{ id = \"ex\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 1, ref = 71 }} }}\n\
             table.insert(skin.destination, {{ id = \"v\", dst = {{ {{ x = 0, y = 900, w = 6, h = 6 }} }} }})\n\
             table.insert(skin.destination, {{ id = \"ex\", dst = {{ {{ x = 0, y = 100, w = 6, h = 6 }} }} }})"
        ),
        random,
    );
    d.tally = tally;
    let glyphs = d.ex_glyphs(1_000);
    d.tally = tally;
    let mut shown: Vec<_> = d
        .frame(1_000, &[])
        .into_iter()
        .filter(|q| q.rect[1] < 500.0)
        .collect();
    shown.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
    shown
        .iter()
        .map(|q| glyphs[&q.uv.map(f32::to_bits)])
        .collect()
}

/// s10-10-design.md C28: the best's and the target's numbers show §8's
/// independent values, by `ref` and read by a callback: 150 the best 11,
/// 151 and 121 the target 13, 152 the best's paced difference 1 (5 −
/// ⌊11·3/7⌋), 153 the target's 0 (5 − ⌊13·3/7⌋), 122/123 the target's
/// rate 92.85 and 183/184 the best's 78.57.
#[test]
fn the_score_figures_show_their_fields() {
    for (id, want) in [
        (150, "11"),
        (151, "13"),
        (121, "13"),
        (152, "1"),
        (153, "0"),
        (122, "92"),
        (123, "85"),
        (183, "78"),
        (184, "57"),
    ] {
        for prop in [
            format!("ref = {id}"),
            format!("value = function() return ms.number({id}) end"),
        ] {
            assert_eq!(score_digits(&prop, judged_five(), false), want, "{prop}");
        }
    }
}

/// s10-10-design.md C28, G15: for a `#RANDOM` chart the best's paced
/// difference and rate draw nothing (`best.valid`), by `ref` and in a
/// callback, while the best itself and the target's figures still show.
#[test]
fn a_random_chart_hides_the_comparisons_with_the_best() {
    for id in [152, 183, 184] {
        for prop in [
            format!("ref = {id}"),
            format!("value = function() return ms.number({id}) end"),
        ] {
            assert_eq!(score_digits(&prop, judged_five(), true), "", "{prop}");
        }
    }
    assert_eq!(score_digits("ref = 150", judged_five(), true), "11");
    assert_eq!(score_digits("ref = 153", judged_five(), true), "0");
}

/// Images `i0` drawn `10 + k` wide under the `k`-th of `ops`, each on
/// its own destination, so a frame's widths read which hold.
fn conditions(ops: &[i32]) -> String {
    ops.iter()
        .enumerate()
        .map(|(k, op)| {
            format!(
                "table.insert(skin.destination, {{ id = \"i0\", op = {{ {op} }},\n\
                   dst = {{ {{ x = 0, y = 0, w = {w}, h = 10 }} }} }})\n",
                w = 10 + k
            )
        })
        .collect()
}

/// s10-10-design.md C28: options 200 to 207 hold the current rate's band
/// and 220 to 227 the whole-chart ranks reached, as the play computes
/// them: none before the first judgment; over §8's progress (5/6 judged,
/// 5/14 whole) only 201 (AA) and 225 to 227.
#[test]
fn the_rank_options_read_the_play_s_ranks() {
    let ops: Vec<i32> = (200..=207).chain(220..=227).collect();
    let mut d = scored(&conditions(&ops), false);
    assert_eq!(read(&mut d), Vec::<i32>::new());
    d.tally = judged_five();
    let on: Vec<i32> = read(&mut d)
        .iter()
        .map(|&k| ops[usize::try_from(k).unwrap()])
        .collect();
    assert_eq!(on, [201, 225, 226, 227]);
}

/// s10-10-design.md C28: timers 351 and 352 start at the update whose EX
/// reaches the best (11) and the target (13): neither at EX 5, the best's
/// alone at 12, both at 13.
#[test]
fn the_best_and_target_timers_start_on_reaching_them() {
    let body = "table.insert(skin.destination, { id = \"i0\", timer = 351,\n\
                  dst = { { x = 0, y = 0, w = 11, h = 10 } } })\n\
                table.insert(skin.destination, { id = \"i0\", timer = 352,\n\
                  dst = { { x = 0, y = 0, w = 12, h = 10 } } })\n";
    let mut d = scored(body, false);
    d.tally = judged_five();
    assert_eq!(read(&mut d), Vec::<i32>::new());
    d.tally = judged_ex(12);
    assert_eq!(read(&mut d), [1]);
    d.tally = judged_ex(13);
    assert_eq!(read(&mut d), [1, 2]);
}

/// A `graph` of `ty` cutting image `i0`'s source 140 px wide from the
/// left, drawn at y `y`.
fn score_graph(ty: i32, y: i32) -> String {
    format!(
        "table.insert(skin.graph, {{ id = \"g{ty}\", src = \"a\", x = 0, y = 0, w = 64, h = 4, type = {ty}, angle = 0 }})\n\
         table.insert(skin.destination, {{ id = \"g{ty}\", dst = {{ {{ x = 0, y = {y}, w = 140, h = 4 }} }} }})\n"
    )
}

/// s10-10-design.md C28, C29: graph types 110 to 115 are ratio bars of
/// the play's score rates, each cut to §8's exact rate of 140 px: 110 5/14,
/// 111 5/6, 112 4/14, 113 11/14, 114 5/14, 115 13/14; type 116 is dropped
/// with the note every graph had.
#[test]
fn score_graphs_cut_to_their_rates() {
    let mut body = String::from("skin.graph = {}\n");
    for (k, ty) in (110..=116).enumerate() {
        body.push_str(&score_graph(ty, 100 + 10 * i32::try_from(k).unwrap()));
    }
    let dir = folder(&body);
    let i = import(&dir).unwrap();
    let dropped: Vec<_> = i
        .imported
        .report
        .notes
        .iter()
        .filter(|n| {
            n.kind
                == NoteKind::Unsupported {
                    object: skin_beatoraja::Object::Graph,
                }
        })
        .collect();
    assert_eq!(dropped.len(), 1, "{:?}", i.imported.report.notes);
    let mut d = scored(&body, false);
    d.tally = judged_five();
    let mut widths: Vec<(i32, f32)> = d
        .frame(1_000, &[])
        .iter()
        .map(|q| (round(q.rect)[1], q.rect[2]))
        .collect();
    // The destinations' `y` is from the top: the first graph is lowest.
    widths.sort_by_key(|&(y, _)| -y);
    let want = [
        140.0 * 5.0 / 14.0,
        140.0 * 5.0 / 6.0,
        140.0 * 4.0 / 14.0,
        140.0 * 11.0 / 14.0,
        140.0 * 5.0 / 14.0,
        140.0 * 13.0 / 14.0,
    ];
    assert_eq!(widths.len(), want.len(), "{widths:?}");
    for ((_, got), want) in widths.iter().zip(want) {
        assert!((got - want).abs() < 1e-3, "{widths:?}");
    }
}

/// s10-10-design.md C28 and design.md "Rendering and Skins": a
/// `float_number` read is taken only as an operand of `==` or `~=` whose
/// other operand is the literal 0 (parentheses aside). Every other use is
/// refused as no expression's type, even where
/// the use would fold to a constant (Lua's truthiness, `not`, arithmetic
/// whose result is only tested for truth), through a local, or against a
/// zero computed from literals.
#[test]
fn other_uses_of_the_best_rate_are_refused() {
    for draw in [
        "return ms.float_number(113)",
        "return not ms.float_number(113)",
        "return ms.float_number(113) + 1",
        "local r = ms.float_number(113) return r == 0",
        "return ms.float_number(113) == (1 - 1)",
    ] {
        let body = format!(
            "table.insert(skin.destination, {{ id = \"i0\",
               draw = function() {draw} end,
               dst = {{ {{ x = 0, y = 0, w = 11, h = 10 }} }} }})
"
        );
        let i = import(&folder(&body)).unwrap();
        assert_eq!(
            i.callbacks,
            [Disposition::Replaced(Unsupported::Type)],
            "{draw}"
        );
        let refused: Vec<NoteKind> = i
            .imported
            .report
            .notes
            .iter()
            .filter(|n| matches!(n.kind, NoteKind::Callback { .. }))
            .map(|n| n.kind)
            .collect();
        assert_eq!(
            refused,
            [NoteKind::Callback {
                field: "draw",
                why: Unsupported::Type
            }],
            "{draw}"
        );
    }
    // The literal may sit in parentheses, on either side.
    let body = "table.insert(skin.destination, { id = \"i0\",
                  draw = function() return (0) ~= (ms.float_number(113)) end,
                  dst = { { x = 0, y = 0, w = 11, h = 10 } } })
";
    let i = import(&folder(body)).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered]);
}

/// s10-10-design.md C28: `float_number(113)` is the best's rate, read
/// against 0: drawn as non-zero with a best (11), as zero with none and
/// for a `#RANDOM` chart (G15). Any other use of it is no expression's
/// type, and another id is unmapped: neither lowers, each follows its
/// role.
#[test]
fn the_best_rate_reads_against_zero() {
    let body = "table.insert(skin.destination, { id = \"i0\",\n\
                  draw = function() return ms.float_number(113) ~= 0 end,\n\
                  dst = { { x = 0, y = 0, w = 11, h = 10 } } })\n\
                table.insert(skin.destination, { id = \"i0\",\n\
                  draw = function() return 0.0 == ms.float_number(113) end,\n\
                  dst = { { x = 0, y = 0, w = 12, h = 10 } } })\n";
    let i = import(&folder(body)).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered, Disposition::Lowered]);
    for (random, best, want) in [(false, 11, [1]), (false, 0, [2]), (true, 11, [2])] {
        let mut d = scored_by(body, random, best, 13);
        d.tally = judged_five();
        assert_eq!(read(&mut d), want, "random {random}, best {best}");
    }
    for (draw, why) in [
        ("ms.float_number(113) > 0", Unsupported::Type),
        ("ms.float_number(113) + 1 == 1", Unsupported::Type),
        (
            "ms.float_number(114) ~= 0",
            Unsupported::Unmapped { api: 2, id: 114 },
        ),
    ] {
        let body = format!(
            "table.insert(skin.destination, {{ id = \"i0\",\n\
               draw = function() return {draw} end,\n\
               dst = {{ {{ x = 0, y = 0, w = 11, h = 10 }} }} }})\n"
        );
        let i = import(&folder(&body)).unwrap();
        assert_eq!(i.callbacks, [Disposition::Replaced(why)], "{draw}");
    }
}
